Texture2D<float4> material : register(t0);
SamplerState pointSampler : register(s0);
RWStructuredBuffer<uint> pixels : register(u0);
cbuffer Tile : register(b0) { uint2 origin; };

// Read the mapped RGBA8 tile as a material would, through its SRV and sampler.
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint width, height;
    material.GetDimensions(width, height);
    float2 uv = (origin + id.xy + 0.5) / float2(width, height);
    uint4 rgba = (uint4)round(saturate(material.SampleLevel(pointSampler, uv, 0)) * 255);
    pixels[id.y * 128 + id.x] = rgba.x | (rgba.y << 8) | (rgba.z << 16) | (rgba.w << 24);
}
