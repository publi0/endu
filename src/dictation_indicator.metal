#include <metal_stdlib>
using namespace metal;

struct Uniforms {
    float2 resolution;
    float time;
    float width;
    float height;
    float opacity;
    float scale;
    float softness;
    float average;
    float peak;
    float processing;
    float post_processing;
    float capturing;
    float editing;
    float queued_count;
    float line_style;
    float line_count;
    float line_curvature;
    float line_speed;
    float line_sharpness;
    float line_glow;
    float sphere_depth;
    float light_angle;
    float sphere_outline;
    float completion;
    float recording_flash;
    float preparing;
    float recording_hue_shift;
    float transcription_hue_shift;
    float brightness;
    float light;
    // 1 hides the lines so a written notice can sit in the capsule.
    float silenced;
};

struct VertexOutput {
    float4 position [[position]];
    float2 uv;
};

vertex VertexOutput indicator_vertex(uint vertex_id [[vertex_id]]) {
    const float2 positions[] = {
        float2(-1.0, -1.0),
        float2( 3.0, -1.0),
        float2(-1.0,  3.0),
    };
    VertexOutput output;
    output.position = float4(positions[vertex_id], 0.0, 1.0);
    output.uv = positions[vertex_id] * float2(0.5, -0.5) + 0.5;
    return output;
}

float rounded_box(float2 point, float2 half_size, float radius) {
    float2 q = abs(point) - half_size + radius;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;
}

float coverage(float distance, float softness) {
    return smoothstep(softness, -softness, distance);
}

float glow(float distance, float radius) {
    float outside = max(distance, 0.0);
    return exp2(-outside * outside / max(radius * radius, 0.001));
}

void composite(thread float4 &destination, float3 color, float alpha) {
    alpha = clamp(alpha, 0.0, 1.0);
    destination.rgb = color * alpha + destination.rgb * (1.0 - alpha);
    destination.a = alpha + destination.a * (1.0 - alpha);
}

void screen(thread float4 &destination, float3 color, float amount) {
    amount = clamp(amount, 0.0, 1.0);
    destination.rgb = 1.0 - (1.0 - destination.rgb) * (1.0 - color * amount);
}

// Rotate hue without changing value or saturation. The defaults
// take the exact identity path, preserving the original recording/orb colors.
float3 rotate_hue(float3 color, float turns) {
    if (abs(turns) < 0.000001) return color;
    float upper = max(color.r, max(color.g, color.b));
    float lower = min(color.r, min(color.g, color.b));
    float chroma = upper - lower;
    if (chroma < 0.000001) return color;
    float sector;
    if (upper == color.r) sector = (color.g - color.b) / chroma;
    else if (upper == color.g) sector = (color.b - color.r) / chroma + 2.0;
    else sector = (color.r - color.g) / chroma + 4.0;
    sector = fract(sector / 6.0 + turns) * 6.0;
    float middle = chroma * (1.0 - abs(fmod(sector, 2.0) - 1.0));
    float3 rotated;
    if (sector < 1.0) rotated = float3(chroma, middle, 0.0);
    else if (sector < 2.0) rotated = float3(middle, chroma, 0.0);
    else if (sector < 3.0) rotated = float3(0.0, chroma, middle);
    else if (sector < 4.0) rotated = float3(0.0, middle, chroma);
    else if (sector < 5.0) rotated = float3(middle, 0.0, chroma);
    else rotated = float3(chroma, 0.0, middle);
    return rotated + lower;
}

// The stroke of y = wave(x): a sine whose amplitude tapers to zero at both
// ends, so the lines grow out of the capsule's middle.
float wave(float x, float start, float end, float amplitude, float frequency, float phase) {
    float progress = clamp((x - start) / max(end - start, 0.001), 0.0, 1.0);
    return amplitude * sin(progress * 3.14159265) * sin(x * frequency + phase);
}

// Slope-corrected distance to the wave, so the stroke keeps its width on the
// steep parts instead of thinning out.
float wave_distance(float2 point, float start, float end, float amplitude, float frequency, float phase) {
    float step = 0.05;
    float y = wave(point.x, start, end, amplitude, frequency, phase);
    float slope = (wave(point.x + step, start, end, amplitude, frequency, phase)
        - wave(point.x - step, start, end, amplitude, frequency, phase)) / (2.0 * step);
    return abs(point.y - y) / sqrt(1.0 + slope * slope);
}

float segment_distance(float2 point, float2 a, float2 b) {
    float2 ab = b - a;
    float along = clamp(dot(point - a, ab) / max(dot(ab, ab), 0.0001), 0.0, 1.0);
    return length(point - a - ab * along);
}

// A dark (or, in light mode, Tabatinga) capsule with two fine lines in the
// phase color: they follow the voice while recording, run quickly while
// transcribing, rest gray while preparing, and give way to a check when done.
fragment float4 indicator_fragment(
    VertexOutput input [[stage_in]],
    constant Uniforms &uniforms [[buffer(0)]])
{
    float backing_scale = uniforms.resolution.x / 112.0;
    float2 point = (input.uv * uniforms.resolution - uniforms.resolution * 0.5)
        / backing_scale / max(uniforms.scale, 0.001);
    float2 half_size = float2(uniforms.width, uniforms.height) * 0.5;
    float radius = uniforms.height * 0.5;
    float distance = rounded_box(point, half_size, radius);
    float lifecycle_softness = max(uniforms.softness, 0.0);
    float edge = (max(fwidth(distance), 0.32) + lifecycle_softness * 0.72)
        / max(uniforms.scale, 0.001);
    float shape = coverage(distance, edge);
    float detail_clarity = exp2(-lifecycle_softness * 0.34);
    bool light_mode = uniforms.light > 0.5;

    float processing = clamp(uniforms.processing, 0.0, 1.0);
    float post_processing = clamp(uniforms.post_processing, 0.0, 1.0);
    float preparing = clamp(uniforms.preparing, 0.0, 1.0);
    float completion = clamp(uniforms.completion, 0.0, 1.0);
    float recording_flash = clamp(uniforms.recording_flash, 0.0, 1.0) * (1.0 - processing);
    float completion_flash = smoothstep(0.0, 0.16, completion)
        * (1.0 - smoothstep(0.32, 1.0, completion));
    float average_power = clamp(uniforms.average, 0.0, 1.0);
    float peak_power = clamp(uniforms.peak, 0.0, 1.0);
    float level = max(average_power, peak_power * 0.6);

    // Palettes rotate these, so Red and Blue stay the defaults.
    float3 recording_color = rotate_hue(
        light_mode ? float3(0.80, 0.16, 0.08) : float3(0.98, 0.30, 0.16),
        uniforms.recording_hue_shift);
    float3 transcription_color = rotate_hue(
        light_mode ? float3(0.16, 0.34, 0.80) : float3(0.47, 0.64, 1.0),
        uniforms.transcription_hue_shift);
    float3 violet = light_mode ? float3(0.42, 0.16, 0.78) : float3(0.70, 0.50, 1.0);
    transcription_color = mix(transcription_color, violet, post_processing);
    float3 resting_color = light_mode ? float3(0.62, 0.60, 0.56) : float3(0.50, 0.51, 0.54);
    float3 line_color = mix(recording_color, transcription_color, processing);
    line_color = mix(line_color, resting_color, preparing);

    float3 fill = light_mode ? float3(0.984, 0.973, 0.945) : float3(0.047, 0.047, 0.055);
    float3 rim_color = light_mode ? float3(0.84, 0.80, 0.71) : float3(1.0);
    float rim_strength = light_mode ? 0.9 : 0.14;
    float glow_strength = light_mode ? 0.55 : 1.0;

    float4 result = 0.0;
    float speaking = (1.0 - processing) * (1.0 - preparing);
    float halo = glow(distance, 5.0 + lifecycle_softness) * (0.06 + 0.3 * level) * speaking
        + glow(distance, 6.0 + lifecycle_softness * 0.5) * completion_flash * 0.3
        + glow(distance, 7.0 + lifecycle_softness * 0.5) * recording_flash * 0.45;
    composite(result, line_color, halo * glow_strength * (1.0 - shape));

    composite(result, fill, shape * (light_mode ? 0.97 : 0.94));
    float rim = coverage(abs(distance) - 0.25, edge);
    composite(result, rim_color, rim * rim_strength * detail_clarity);
    composite(result, recording_color, rim * recording_flash * 0.7 * detail_clarity);

    float inset = radius * 0.9;
    float start = -half_size.x + inset;
    float end = half_size.x - inset;
    float time = uniforms.time;
    float rest = 0.5 + 0.5 * sin(time * 4.0);
    float amplitude = mix(0.6 + 4.2 * level, 1.5, processing);
    amplitude = mix(amplitude, 0.35 + 0.25 * rest, preparing);
    amplitude *= 1.0 - smoothstep(0.0, 0.5, completion);
    float speed = mix(mix(5.0, 9.0, processing), 2.0, preparing);
    float frequency = 0.27;
    float primary = wave_distance(point, start, end, amplitude, frequency, time * speed);
    float secondary = wave_distance(point, start, end, amplitude * 0.7, frequency, time * speed + 2.2);
    float span = smoothstep(start - 0.6, start + 0.6, point.x)
        * (1.0 - smoothstep(end - 0.6, end + 0.6, point.x));
    float lines_visible = shape * span * detail_clarity
        * (1.0 - smoothstep(0.3, 0.7, completion))
        * (preparing > 0.5 ? 0.5 + 0.3 * rest : 1.0)
        * (1.0 - clamp(uniforms.silenced, 0.0, 1.0));
    float stroke_edge = 0.3 / max(uniforms.scale, 0.001);
    float line_glow = glow(primary, 1.6) * 0.35 + glow(secondary, 1.4) * 0.18;
    composite(result, line_color, line_glow * glow_strength * lines_visible * (1.0 - preparing));
    composite(result, line_color, coverage(secondary - 0.32, stroke_edge) * 0.55 * lines_visible);
    composite(result, line_color, coverage(primary - 0.45, stroke_edge) * lines_visible);

    if (completion > 0.0) {
        float drawn = smoothstep(0.25, 0.75, completion);
        float2 a = float2(-3.2, 0.2);
        float2 b = float2(-1.0, 2.4);
        float2 c = float2(3.4, -2.4);
        float check = min(segment_distance(point, a, b), segment_distance(point, b, c));
        composite(result, line_color, coverage(check - 0.55, stroke_edge) * drawn * shape * detail_clarity);
    }

    // Pending jobs sit beside the foreground state, stationary so queue
    // status never reads as part of the lines.
    float queued_count = min(uniforms.queued_count, 4.0);
    for (int index = 0; index < 4; index++) {
        if (float(index) >= queued_count) {
            break;
        }
        float2 center = float2(uniforms.width * 0.5 + 10.0 + float(index) * 3.4, 0.0);
        float dot_distance = length(point - center) - 0.68;
        float dot = coverage(dot_distance, 0.22);
        float leading_processing = index == 0
            ? post_processing * clamp(uniforms.capturing, 0.0, 1.0)
            : 0.0;
        float3 dot_color = mix(transcription_color, violet, leading_processing);
        composite(result, dot_color, dot * (index == 0 ? 0.92 : 0.52));
    }

    if (uniforms.brightness != 1.0) result.rgb *= uniforms.brightness;
    result *= uniforms.opacity;
    return result;
}
