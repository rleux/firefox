/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "DMABUFTextureClientOGL.h"

#include "gfxPlatform.h"
#include "mozilla/widget/DMABufSurface.h"

namespace mozilla::layers {

using namespace gfx;

DMABUFTextureData::DMABUFTextureData(DMABufSurface* aSurface,
                                     BackendType aBackend)
    : mSurface(aSurface), mBackend(aBackend) {
  MOZ_ASSERT(mSurface);
  if (auto* yuv = mSurface->GetAsDMABufSurfaceYUV()) {
    if (yuv->GetVAAPIDescriptor()) {
      mSurface->GlobalRefAdd();
      mHasGlobalRef = true;
    }
  }
}

DMABUFTextureData::~DMABUFTextureData() { ReleaseSurface(); }

bool DMABUFTextureData::Serialize(SurfaceDescriptor& aOutDescriptor) {
  return mSurface->Serialize(aOutDescriptor);
}

void DMABUFTextureData::FillInfo(TextureData::Info& aInfo) const {
  aInfo.size = gfx::IntSize(mSurface->GetWidth(), mSurface->GetHeight());
  aInfo.format = mSurface->GetFormat();
  aInfo.hasSynchronization = false;
  aInfo.supportsMoz2D = false;
  aInfo.canExposeMappedData = false;
}

bool DMABUFTextureData::Lock(OpenMode) {
  MOZ_DIAGNOSTIC_CRASH("Not implemented.");
  return false;
}

void DMABUFTextureData::Unlock() { MOZ_DIAGNOSTIC_CRASH("Not implemented."); }

already_AddRefed<DataSourceSurface> DMABUFTextureData::GetAsSurface() {
  // TODO: Update for debug purposes.
  return nullptr;
}

void DMABUFTextureData::ReleaseSurface() {
  if (mHasGlobalRef) {
    mSurface->GlobalRefRelease();
    mHasGlobalRef = false;
  }
  mSurface = nullptr;
}

void DMABUFTextureData::Deallocate(LayersIPCChannel*) { ReleaseSurface(); }

void DMABUFTextureData::Forget(LayersIPCChannel*) { ReleaseSurface(); }

}  // namespace mozilla::layers
