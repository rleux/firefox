/*
Copyright 2019 WebGPU Samples Contributors

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

   1. Redistributions of source code must retain the above copyright notice,
      this list of conditions and the following disclaimer.

   2. Redistributions in binary form must reproduce the above copyright notice,
      this list of conditions and the following disclaimer in the documentation
      and/or other materials provided with the distribution.

   3. Neither the name of the copyright holder nor the names of its
      contributors may be used to endorse or promote products derived from this
      software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Source: https://github.com/webgpu/webgpu-samples/blob/e040ec1a20dbbe01afed30c812e825b2639cfe5b/sample/animometer/animometer.wgsl
*/
struct Time {
  value : f32,
}

struct Uniforms {
  scale : f32,
  offsetX : f32,
  offsetY : f32,
  scalar : f32,
  scalarOffset : f32,
}

@binding(0) @group(0) var<uniform> time : Time;
@binding(0) @group(1) var<uniform> uniforms : Uniforms;

struct VertexOutput {
  @builtin(position) Position : vec4f,
  @location(0) v_color : vec4f,
}

@vertex
fn vert_main(
  @location(0) position : vec4f,
  @location(1) color : vec4f
) -> VertexOutput {
  var fade = (uniforms.scalarOffset + time.value * uniforms.scalar / 10.0) % 1.0;
  if (fade < 0.5) {
    fade = fade * 2.0;
  } else {
    fade = (1.0 - fade) * 2.0;
  }
  var xpos = position.x * uniforms.scale;
  var ypos = position.y * uniforms.scale;
  var angle = 3.14159 * 2.0 * fade;
  var xrot = xpos * cos(angle) - ypos * sin(angle);
  var yrot = xpos * sin(angle) + ypos * cos(angle);
  xpos = xrot + uniforms.offsetX;
  ypos = yrot + uniforms.offsetY;

  return VertexOutput(
    vec4(xpos, ypos, 0.0, 1.0),
    vec4(fade, 1.0 - fade, 0.0, 1.0) + color,
  );
}

@fragment
fn frag_main(@location(0) v_color : vec4f) -> @location(0) vec4f {
  return v_color;
}
