// Encodes a finished mono 16 kHz clip to raw Opus packets with the system
// AudioToolbox encoder. Rust muxes the packets into Ogg (src/providers/opus.rs).

#include <AudioToolbox/AudioToolbox.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

enum { SAMPLE_RATE = 16000, FRAMES_PER_PACKET = 320, PACKETS_PER_CALL = 32 };

typedef struct {
    const float *samples;
    size_t remaining;
} Source;

static OSStatus provide(AudioConverterRef converter, UInt32 *packets, AudioBufferList *data,
                        AudioStreamPacketDescription **descriptions, void *context) {
    (void)converter;
    (void)descriptions;
    Source *source = context;
    UInt32 count = *packets;
    if (count > source->remaining) {
        count = (UInt32)source->remaining;
    }
    // Zero packets with noErr is the end of input; the converter flushes.
    data->mBuffers[0].mData = count ? (void *)source->samples : NULL;
    data->mBuffers[0].mDataByteSize = count * (UInt32)sizeof(float);
    data->mBuffers[0].mNumberChannels = 1;
    source->samples += count;
    source->remaining -= count;
    *packets = count;
    return noErr;
}

static int grow(void **buffer, size_t *capacity, size_t needed, size_t item) {
    if (needed <= *capacity) {
        return 1;
    }
    size_t next = *capacity ? *capacity : 64;
    while (next < needed) {
        next *= 2;
    }
    void *resized = realloc(*buffer, next * item);
    if (!resized) {
        return 0;
    }
    *buffer = resized;
    *capacity = next;
    return 1;
}

// On success returns 0 and hands the caller `data` (concatenated packets) and
// `sizes` (one length per packet), both released with endu_opus_free.
int endu_opus_encode(const float *samples, size_t count, uint32_t bitrate, uint8_t **data,
                     size_t *data_len, uint32_t **sizes, size_t *packet_count,
                     uint32_t *priming) {
    *data = NULL;
    *sizes = NULL;
    *data_len = 0;
    *packet_count = 0;
    *priming = 0;

    AudioStreamBasicDescription input = {0};
    input.mSampleRate = SAMPLE_RATE;
    input.mFormatID = kAudioFormatLinearPCM;
    input.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
    input.mBytesPerPacket = sizeof(float);
    input.mFramesPerPacket = 1;
    input.mBytesPerFrame = sizeof(float);
    input.mChannelsPerFrame = 1;
    input.mBitsPerChannel = 32;

    AudioStreamBasicDescription output = {0};
    output.mSampleRate = SAMPLE_RATE;
    output.mFormatID = kAudioFormatOpus;
    output.mFramesPerPacket = FRAMES_PER_PACKET;
    output.mChannelsPerFrame = 1;

    AudioConverterRef converter = NULL;
    OSStatus status = AudioConverterNew(&input, &output, &converter);
    if (status != noErr) {
        return (int)status;
    }
    UInt32 rate = bitrate;
    status = AudioConverterSetProperty(converter, kAudioConverterEncodeBitRate, sizeof rate, &rate);
    // The default quality is about 3.5 times slower for the same size; the
    // encode sits between Finish and the upload it is meant to shorten.
    UInt32 quality = kAudioConverterQuality_Min;
    if (status == noErr) {
        status = AudioConverterSetProperty(converter, kAudioConverterCodecQuality, sizeof quality,
                                           &quality);
    }
    UInt32 max_packet = 0;
    UInt32 size = sizeof max_packet;
    if (status == noErr) {
        status = AudioConverterGetProperty(
            converter, kAudioConverterPropertyMaximumOutputPacketSize, &size, &max_packet);
    }
    if (status != noErr || max_packet == 0) {
        AudioConverterDispose(converter);
        return status != noErr ? (int)status : -1;
    }
    // Without prime info the stream keeps pre-skip 0: every sample still
    // decodes, the encoder delay plays as leading silence and the final
    // granule trims that much from the end. Guessing libopus's lookahead
    // could cut speech if this encoder's delay differed.
    AudioConverterPrimeInfo prime = {0};
    size = sizeof prime;
    if (AudioConverterGetProperty(converter, kAudioConverterPrimeInfo, &size, &prime) == noErr) {
        *priming = prime.leadingFrames;
    }

    uint8_t *chunk = malloc((size_t)max_packet * PACKETS_PER_CALL);
    AudioStreamPacketDescription descriptions[PACKETS_PER_CALL];
    size_t data_capacity = 0;
    size_t sizes_capacity = 0;
    Source source = {samples, count};
    int result = chunk ? 0 : -1;
    while (result == 0) {
        AudioBufferList list;
        list.mNumberBuffers = 1;
        list.mBuffers[0].mNumberChannels = 1;
        list.mBuffers[0].mDataByteSize = max_packet * PACKETS_PER_CALL;
        list.mBuffers[0].mData = chunk;
        UInt32 packets = PACKETS_PER_CALL;
        status = AudioConverterFillComplexBuffer(converter, provide, &source, &packets, &list,
                                                 descriptions);
        if (status != noErr) {
            result = (int)status;
            break;
        }
        if (packets == 0) {
            break;
        }
        for (UInt32 index = 0; index < packets && result == 0; index++) {
            AudioStreamPacketDescription packet = descriptions[index];
            if (packet.mStartOffset < 0 ||
                (UInt64)packet.mStartOffset + packet.mDataByteSize >
                    (UInt64)max_packet * PACKETS_PER_CALL) {
                result = -1;
                break;
            }
            if (!grow((void **)data, &data_capacity, *data_len + packet.mDataByteSize, 1) ||
                !grow((void **)sizes, &sizes_capacity, *packet_count + 1, sizeof(uint32_t))) {
                result = -1;
                break;
            }
            memcpy(*data + *data_len, chunk + packet.mStartOffset, packet.mDataByteSize);
            *data_len += packet.mDataByteSize;
            (*sizes)[(*packet_count)++] = packet.mDataByteSize;
        }
    }
    free(chunk);
    AudioConverterDispose(converter);
    if (result != 0 || *packet_count == 0) {
        free(*data);
        free(*sizes);
        *data = NULL;
        *sizes = NULL;
        *data_len = 0;
        *packet_count = 0;
        return result != 0 ? result : -1;
    }
    return 0;
}

void endu_opus_free(void *pointer) { free(pointer); }

// Test support only: decodes raw packets back to 16 kHz float with the system
// Opus decoder so unit tests can prove the stream round-trips. The app never
// calls it and the linker strips it from release builds.
typedef struct {
    const uint8_t *data;
    const uint32_t *sizes;
    size_t count;
    size_t next;
    size_t offset;
    AudioStreamPacketDescription description;
} PacketSource;

static OSStatus provide_packet(AudioConverterRef converter, UInt32 *packets,
                               AudioBufferList *data,
                               AudioStreamPacketDescription **descriptions, void *context) {
    (void)converter;
    PacketSource *source = context;
    if (source->next >= source->count) {
        data->mBuffers[0].mData = NULL;
        data->mBuffers[0].mDataByteSize = 0;
        *packets = 0;
        return noErr;
    }
    UInt32 size = source->sizes[source->next];
    data->mBuffers[0].mData = (void *)(source->data + source->offset);
    data->mBuffers[0].mDataByteSize = size;
    data->mBuffers[0].mNumberChannels = 1;
    if (descriptions) {
        source->description.mStartOffset = 0;
        source->description.mVariableFramesInPacket = 0;
        source->description.mDataByteSize = size;
        *descriptions = &source->description;
    }
    source->offset += size;
    source->next += 1;
    *packets = 1;
    return noErr;
}

int endu_opus_decode_for_tests(const uint8_t *data, const uint32_t *sizes, size_t count,
                               float *out, size_t capacity, size_t *written) {
    *written = 0;
    AudioStreamBasicDescription input = {0};
    input.mSampleRate = SAMPLE_RATE;
    input.mFormatID = kAudioFormatOpus;
    input.mFramesPerPacket = FRAMES_PER_PACKET;
    input.mChannelsPerFrame = 1;

    AudioStreamBasicDescription output = {0};
    output.mSampleRate = SAMPLE_RATE;
    output.mFormatID = kAudioFormatLinearPCM;
    output.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;
    output.mBytesPerPacket = sizeof(float);
    output.mFramesPerPacket = 1;
    output.mBytesPerFrame = sizeof(float);
    output.mChannelsPerFrame = 1;
    output.mBitsPerChannel = 32;

    AudioConverterRef converter = NULL;
    OSStatus status = AudioConverterNew(&input, &output, &converter);
    if (status != noErr) {
        return (int)status;
    }
    // Output every decoded sample, as an Ogg Opus decoder does before it
    // applies pre-skip; by default the converter trims part of the delay.
    AudioConverterPrimeInfo none = {0};
    status = AudioConverterSetProperty(converter, kAudioConverterPrimeInfo, sizeof none, &none);
    if (status != noErr) {
        AudioConverterDispose(converter);
        return (int)status;
    }
    PacketSource source = {data, sizes, count, 0, 0, {0}};
    while (*written < capacity) {
        UInt32 frames = (UInt32)(capacity - *written);
        AudioBufferList list;
        list.mNumberBuffers = 1;
        list.mBuffers[0].mNumberChannels = 1;
        list.mBuffers[0].mDataByteSize = frames * (UInt32)sizeof(float);
        list.mBuffers[0].mData = out + *written;
        status = AudioConverterFillComplexBuffer(converter, provide_packet, &source, &frames, &list,
                                                 NULL);
        if (status != noErr) {
            AudioConverterDispose(converter);
            return (int)status;
        }
        if (frames == 0) {
            break;
        }
        *written += frames;
    }
    AudioConverterDispose(converter);
    return 0;
}
