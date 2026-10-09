/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "gfxPlatform.h"
#include "gtest/gtest.h"
#include "mozilla/ScopeExit.h"
#include "mozilla/X11Util.h"
#include "mozilla/layers/SynchronousTask.h"
#include "mozilla/webrender/RenderCompositorVulkan.h"
#include "mozilla/webrender/RenderThread.h"
#include "mozilla/widget/GtkCompositorWidget.h"
#include "mozilla/widget/PlatformWidgetTypes.h"
#include "nsThreadUtils.h"
#include "nsWindow.h"

namespace mozilla::wr {

class TestGtkCompositorWidget final : public widget::GtkCompositorWidget {
 public:
  explicit TestGtkCompositorWidget(
      const widget::GtkCompositorWidgetInitData& aData)
      : GtkCompositorWidget(aData, layers::CompositorOptions(false, true),
                            nullptr) {}
  void ObserveVsync(VsyncObserver*) override {}
};

class VulkanX11Surface : public ::testing::Test {
 protected:
  void SetUp() override {
    ASSERT_TRUE(gtk_init_check(nullptr, nullptr));
    ASSERT_NE(DefaultXDisplay(), nullptr);
    gfxPlatform::GetPlatform();
    gfxPlatform::InitLayersIPC();
    ASSERT_NE(RenderThread::Get(), nullptr);
  }

  template <typename F>
  void OnRenderThread(F&& aTest) {
    layers::SynchronousTask task("Vulkan X11 surface test");
    nsCOMPtr<nsIThread> thread = RenderThread::GetRenderThread();
    ASSERT_EQ(NS_OK,
              thread->Dispatch(NS_NewRunnableFunction(
                                   "Vulkan X11 surface test",
                                   [test = std::move(aTest), &task] {
                                     layers::AutoCompleteTask complete(&task);
                                     test();
                                   }),
                               NS_DISPATCH_NORMAL));
    task.Wait();
  }

  static RefPtr<widget::CompositorWidget> Widget(Window aWindow,
                                                 bool aNeedsAlpha = false) {
    widget::GtkCompositorWidgetInitData data(
        aWindow, nsCString(XDisplayString(DefaultXDisplay())), true,
        aNeedsAlpha, LayoutDeviceIntSize(64, 48));
    return new TestGtkCompositorWidget(data);
  }
};

// Enable explicitly with an X11 display; desktop gtests otherwise run headless.
TEST_F(VulkanX11Surface, DISABLED_RetainedDisplayOutlivesCompositor) {
  auto* display = DefaultXDisplay();
  auto window = XCreateSimpleWindow(display, DefaultRootWindow(display), 0, 0,
                                    64, 48, 0, 0, 0);
  ASSERT_NE(window, 0UL);
  auto destroy = MakeScopeExit([&] {
    XDestroyWindow(display, window);
    XSync(display, false);
  });
  XSync(display, false);
  OnRenderThread([window, display] {
    auto widget = Widget(window);
    nsCString error;
    auto compositor = RenderCompositorVulkan::Create(widget, error);
    ASSERT_TRUE(compositor)
    << error.get();
    const auto* config = compositor->GetVulkanConfig();
    ASSERT_NE(config, nullptr);
    ASSERT_TRUE(config->window.IsXlib());
    EXPECT_EQ(config->window.xlib.window, window);
    EXPECT_EQ(config->window_owner.object, widget.get());
    auto* ownedDisplay = static_cast<Display*>(config->window.xlib.display);
    EXPECT_NE(ownedDisplay, display);
    auto owner = config->display_owner;
    owner.retain(owner.object);
    auto release = MakeScopeExit([&] { owner.release(owner.object); });
    compositor = nullptr;
    widget = nullptr;
    XWindowAttributes attributes{};
    EXPECT_NE(XGetWindowAttributes(ownedDisplay, window, &attributes), 0);
    EXPECT_EQ(attributes.width, 64);
    EXPECT_EQ(attributes.height, 48);
  });
}

TEST_F(VulkanX11Surface, DISABLED_RejectsUnavailableWindow) {
  OnRenderThread([] {
    auto widget = Widget(0);
    nsCString error;
    EXPECT_EQ(RenderCompositorVulkan::Create(widget, error), nullptr);
    EXPECT_FALSE(error.IsEmpty());
  });
}

TEST_F(VulkanX11Surface, DISABLED_TransparencyRequirementIsIndependentOfDepth) {
  auto* display = DefaultXDisplay();
  XVisualInfo visual = {};
  if (!XMatchVisualInfo(display, DefaultScreen(display), 32, TrueColor,
                        &visual)) {
    GTEST_SKIP() << "No 32-bit X11 visual";
  }
  XSetWindowAttributes attributes = {};
  attributes.colormap = XCreateColormap(display, DefaultRootWindow(display),
                                        visual.visual, AllocNone);
  auto freeColormap =
      MakeScopeExit([&] { XFreeColormap(display, attributes.colormap); });
  auto window = XCreateWindow(display, DefaultRootWindow(display), 0, 0, 64, 48,
                              0, 32, InputOutput, visual.visual,
                              CWColormap | CWBorderPixel, &attributes);
  auto destroy = MakeScopeExit([&] {
    XDestroyWindow(display, window);
    XSync(display, false);
  });
  XSync(display, false);
  for (bool needsAlpha : {false, true}) {
    OnRenderThread([window, needsAlpha] {
      auto widget = Widget(window, needsAlpha);
      nsCString error;
      auto compositor = RenderCompositorVulkan::Create(widget, error);
      ASSERT_TRUE(compositor)
      << error.get();
      const auto* config = compositor->GetVulkanConfig();
      ASSERT_NE(config, nullptr);
      EXPECT_EQ(config->transparent, needsAlpha);
    });
  }
}

}  // namespace mozilla::wr
