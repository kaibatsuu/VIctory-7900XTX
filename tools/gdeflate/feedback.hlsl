// One known sample, to make the resolved region and mip independently testable.
Texture2D<float> streamed : register(t0);
FeedbackTexture2D<SAMPLER_FEEDBACK_MIN_MIP> feedback : register(u0);
SamplerState pointSampler : register(s0);
[numthreads(1, 1, 1)]
void main() {
#if defined(FX1_DIAGONAL_TILES)
    feedback.WriteSamplerFeedbackLevel(streamed, pointSampler, float2(0.25, 0.25), 0);
    feedback.WriteSamplerFeedbackLevel(streamed, pointSampler, float2(0.75, 0.75), 0);
#elif defined(FX1_SECOND_TILE)
    feedback.WriteSamplerFeedbackLevel(streamed, pointSampler, float2(0.25, 0.75), 0);
#else
    feedback.WriteSamplerFeedbackLevel(streamed, pointSampler, float2(0.75, 0.25), 0);
#endif
}
