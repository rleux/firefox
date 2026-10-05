/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "SharedSurfaceDMABUF.h"

#include "GLContextEGL.h"
#include "MozFramebuffer.h"
#include "gfxPlatform.h"
#include "mozilla/gfx/gfxVars.h"
#include "mozilla/layers/LayersSurfaces.h"  // for SurfaceDescriptor, etc
#include "mozilla/widget/DMABufDevice.h"
#include "mozilla/widget/DMABufFormats.h"
#ifdef XP_LINUX
#  include "mozilla/webrender/VulkanImageIPC.h"
#  include "mozilla/webrender/WebRenderTypes.h"
#endif

namespace mozilla::gl {

static bool UseForeignRGB() {
#ifdef XP_LINUX
  return gfx::gfxVars::UseWebRenderVulkan() &&
         !gfx::gfxVars::UseSoftwareWebRender();
#else
  return false;
#endif
}

#ifdef XP_LINUX
static bool SupportsForeignRGB(GLContext* aGL, const gfx::IntSize& aSize) {
  const auto& egl = GLContextEGL::Cast(aGL)->mEgl;
  if (!egl->IsExtensionSupported(EGLExtension::KHR_fence_sync) ||
      !egl->IsExtensionSupported(EGLExtension::ANDROID_native_fence_sync) ||
      aSize.IsEmpty()) {
    return false;
  }
  widget::DMABufDeviceLock device;
  uint64_t drmMajor, drmMinor;
  if (!device.GetDRMDeviceId(drmMajor, drmMinor)) {
    return false;
  }
  wr::WrVulkanForeignRgbDescriptor image{};
  image.fd = -1;
  image.width = aSize.width;
  image.height = aSize.height;
  image.fourcc = GBM_FORMAT_ARGB8888;
  image.stride = uint64_t(aSize.width) * 4;
  return wr::VulkanImageCapabilities::SupportsForeignRGB(image, drmMajor,
                                                         drmMinor);
}
#endif

/*static*/
UniquePtr<SharedSurface_DMABUF> SharedSurface_DMABUF::Create(
    const SharedSurfaceDesc& desc) {
  RefPtr<DMABufSurface> surface;
  UniquePtr<MozFramebuffer> fb;

  const auto flags = static_cast<DMABufSurfaceFlags>(
      DMABUF_SCANOUT | DMABUF_TEXTURE | DMABUF_USE_MODIFIERS | DMABUF_ALPHA);
  const bool foreign = UseForeignRGB();
  RefPtr<widget::DRMFormat> format;
#ifdef XP_LINUX
  if (foreign) {
    if (!SupportsForeignRGB(desc.gl, desc.size)) {
      return nullptr;
    }
    format = new widget::DRMFormat(GBM_FORMAT_ARGB8888, uint64_t(0));
  }
#endif
  surface = DMABufSurfaceRGBA::CreateDMABufSurface(
      desc.gl, desc.size.width, desc.size.height, flags, format);
  if (surface && foreign && !surface->EnableForeignRGB()) {
    return nullptr;
  }
  if (!surface || !surface->CreateTextures(desc.gl)) {
    return nullptr;
  }
  const auto tex = surface->GetTexture();
  fb = MozFramebuffer::CreateForBacking(desc.gl, desc.size, 0, false, false,
                                        LOCAL_GL_TEXTURE_2D, tex);
  if (!fb) return nullptr;

  return AsUnique(new SharedSurface_DMABUF(desc, std::move(fb), surface));
}

SharedSurface_DMABUF::SharedSurface_DMABUF(const SharedSurfaceDesc& desc,
                                           UniquePtr<MozFramebuffer> fb,
                                           const RefPtr<DMABufSurface> surface)
    : SharedSurface(desc, std::move(fb)), mSurface(surface) {}

SharedSurface_DMABUF::~SharedSurface_DMABUF() {
  const auto& gl = mDesc.gl;
  if (!gl || !gl->MakeCurrent()) {
    return;
  }
  mSurface->ReleaseTextures();
}

void SharedSurface_DMABUF::ProducerReleaseImpl() { mSurface->FenceSet(); }

void SharedSurface_DMABUF::WaitForBufferOwnership() { mSurface->FenceWait(); }

Maybe<layers::SurfaceDescriptor> SharedSurface_DMABUF::ToSurfaceDescriptor() {
  layers::SurfaceDescriptor desc;
  if (!mSurface->Serialize(desc)) return {};
  return Some(desc);
}

/*static*/
UniquePtr<SurfaceFactory_DMABUF> SurfaceFactory_DMABUF::Create(GLContext& gl) {
  if (!widget::DMABufDevice::IsDMABufWebGLEnabled()) {
    return nullptr;
  }
#ifdef XP_LINUX
  if (UseForeignRGB() && !SupportsForeignRGB(&gl, gfx::IntSize(1, 1))) {
    return nullptr;
  }
#endif

  auto dmabufFactory = MakeUnique<SurfaceFactory_DMABUF>(gl);
  if (dmabufFactory->CanCreateSurface(gl)) {
    return dmabufFactory;
  }

  LOGDMABUF(
      ("SurfaceFactory_DMABUF::Create() failed, fallback to SW buffers.\n"));
  widget::DMABufDevice::DisableDMABufWebGL();
  return nullptr;
}

bool SurfaceFactory_DMABUF::CanCreateSurface(GLContext& gl) {
  UniquePtr<SharedSurface> test =
      CreateShared(gfx::IntSize(1, 1), gfx::ColorSpace2::SRGB);
  if (!test) {
    LOGDMABUF((
        "SurfaceFactory_DMABUF::CanCreateSurface() failed to create surface."));
    return false;
  }
  auto desc = test->ToSurfaceDescriptor();
  if (!desc) {
    LOGDMABUF(
        ("SurfaceFactory_DMABUF::CanCreateSurface() failed to serialize "
         "surface."));
    return false;
  }
  RefPtr<DMABufSurface> importedSurface =
      DMABufSurface::CreateDMABufSurface(*desc);
  if (!importedSurface) {
    LOGDMABUF((
        "SurfaceFactory_DMABUF::CanCreateSurface() failed to import surface."));
    return false;
  }
  if (!importedSurface->CreateTextures(&gl)) {
    LOGDMABUF(
        ("SurfaceFactory_DMABUF::CanCreateSurface() failed to create texture "
         "over surface."));
    return false;
  }
  return true;
}

SurfaceFactory_DMABUF::SurfaceFactory_DMABUF(GLContext& gl)
    : SurfaceFactory({&gl, SharedSurfaceType::EGLSurfaceDMABUF,
                      layers::TextureType::DMABUF, true}) {}
}  // namespace mozilla::gl
