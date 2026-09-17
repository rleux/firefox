/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include <cstdlib>

#include "gtest/gtest.h"
#include "mozilla/ScopeExit.h"
#include "mozilla/gfx/gfxVars.h"
#include "mozilla/webrender/RenderCompositorVulkan.h"
#include "nsString.h"

TEST(RenderCompositorVulkan, SoftwareOverridesEnvironment)
{
  mozilla::gfx::gfxVars::Initialize();
  const bool software = mozilla::gfx::gfxVars::UseSoftwareWebRender();
  const char* backend = getenv("MOZ_WR_BACKEND");
  const bool hadBackend = backend != nullptr;
  const nsCString savedBackend(backend);
  auto restore = mozilla::MakeScopeExit([&] {
    if (hadBackend) {
      setenv("MOZ_WR_BACKEND", savedBackend.get(), 1);
    } else {
      unsetenv("MOZ_WR_BACKEND");
    }
    mozilla::gfx::gfxVars::SetUseSoftwareWebRender(software);
  });

  ASSERT_EQ(setenv("MOZ_WR_BACKEND", "vulkan", 1), 0);
  mozilla::gfx::gfxVars::SetUseSoftwareWebRender(false);
  EXPECT_TRUE(mozilla::wr::RenderCompositorVulkan::IsRequested());
  mozilla::gfx::gfxVars::SetUseSoftwareWebRender(true);
  EXPECT_FALSE(mozilla::wr::RenderCompositorVulkan::IsRequested());
  mozilla::gfx::gfxVars::SetUseSoftwareWebRender(false);
  EXPECT_TRUE(mozilla::wr::RenderCompositorVulkan::IsRequested());
}
