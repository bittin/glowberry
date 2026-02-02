// [SHADER]
// name: Rainbow Bar
// author: @Krischan (adapted for GlowBerry)
// source: https://www.shadertoy.com/view/7dVXzd
// license: CC BY-NC-SA 3.0
//
// [PARAMS]
// speed: f32 = 1.0 | min: 0.2 | max: 3.0 | step: 0.1 | label: Speed
// height: f32 = 0.05 | min: 0.01 | max: 0.2 | step: 0.01 | label: Bar Height
// backlight: f32 = 0.6 | min: 0.0 | max: 1.0 | step: 0.1 | label: Backlight
// brightness: f32 = 0.33 | min: 0.1 | max: 1.0 | step: 0.05 | label: Brightness
// glow: f32 = 1.25 | min: 0.5 | max: 2.0 | step: 0.1 | label: Glow
// [/PARAMS]

// Default parameter values
const speed: f32 = 1.0;
const height: f32 = 0.05;
const backlight: f32 = 0.6;
const brightness: f32 = 0.33;
const glow: f32 = 1.25;

@fragment
fn main(@builtin(position) fragCoord: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = fragCoord.xy / iResolution - 0.5;
    
    var c = backlight;
    let a = abs(uv.y);
    let s = 1.0 - smoothstep(0.0, height, a);
    c *= 1.33 - smoothstep(0.0, 0.5, a);
    c = c * c * c;
    
    if (abs(uv.y) < height) {
        c += s;
    }
    
    let rainbow = cos(6.283 * (uv.x + iTime * speed + vec3<f32>(0.0, 0.33, 0.66))) + glow;
    return vec4<f32>(rainbow * c * brightness, 1.0);
}
