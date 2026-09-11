These supplementary scenes compare retained GL and HAL/Vulkan at 257x129.
`script/test_hal_pixels.py SCENE GL.png HAL.png` checks alpha/depth ordering,
odd readback pitch, image filtering, transforms and rectangular clipping.

Alpha/depth and nearest-filtered pixels must match exactly. In odd-transform,
only the linear-sampled image region (x=132..237, y=16..90) permits a difference
of 2/255 in RGB; alpha and all pixels outside that region remain exact. The
reference OSMesa and Lavapipe runs differ by up to two RGB levels in that region;
optimized and unoptimized GL agree. This isolates filtering/quantization
precision without changing any existing reftest reference or tolerance.

The Vulkan image update/resize test is
`hal::tests::retained_image_updates_and_resize` in Wrench.
