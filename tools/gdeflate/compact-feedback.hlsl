Texture2D<uint> minMip : register(t0);
RWStructuredBuffer<uint> requested : register(u0);
cbuffer TileGeometry : register(b0) { uint2 regionsPerTile; };
groupshared uint tileMask;

// Compact mip-zero demand into one bit per tile. Each group owns an output word.
[numthreads(32, 1, 1)]
void main(uint lane : SV_GroupIndex, uint3 group : SV_GroupID) {
    if (lane == 0) tileMask = 0;
    GroupMemoryBarrierWithGroupSync();
    uint2 dims;
    minMip.GetDimensions(dims.x, dims.y);
    uint2 tiles = (dims + regionsPerTile - 1) / regionsPerTile;
    uint tile = group.x * 32 + lane;
    if (tile < tiles.x * tiles.y) {
        uint2 origin = uint2(tile % tiles.x, tile / tiles.x) * regionsPerTile;
        bool needed = false;
        for (uint y = origin.y; y < min(origin.y + regionsPerTile.y, dims.y) && !needed; ++y)
            for (uint x = origin.x; x < min(origin.x + regionsPerTile.x, dims.x); ++x)
                if (minMip.Load(int3(x, y, 0)) == 0) { needed = true; break; }
        if (needed) InterlockedOr(tileMask, 1u << lane);
    }
    GroupMemoryBarrierWithGroupSync();
    if (lane == 0) requested[group.x] = tileMask;
}
