package com.demoz.hubbridge

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import org.json.JSONObject
import java.util.concurrent.atomic.AtomicLong

/**
 * The H.264 decoder of one mirror, in low-latency mode where the phone's decoder has it,
 * rendering straight onto the monitor's surface.
 *
 * [queue] is called from one thread at a time; a thread of its own takes the pictures out and
 * shows the newest. [release] may come from any thread. After an error the decoder is set up
 * again and waits for a keyframe (which carries the parameter sets).
 */
class MirrorDecoder private constructor(
    private val name: String,
    private val format: MediaFormat,
    private val surface: Surface,
    private val onOutputError: () -> Unit,
    private val onFailed: () -> Unit,
) {
    private val inputLock = Object()
    private var codec: MediaCodec? = null
    private var output: Thread? = null
    @Volatile private var running = false
    @Volatile private var broken = false
    private var released = false

    private val decoded = AtomicLong()
    private val dropped = AtomicLong()
    private val bad = AtomicLong()
    private val queuedAt = DecodeTimes()
    private val outputErrors = OutputRecovery()
    private val timeSumUs = AtomicLong()
    private val timed = AtomicLong()
    @Volatile private var decodeUs = 0L
    private var averagedAt = System.nanoTime()

    private fun open() {
        val c = MediaCodec.createByCodecName(name)
        try {
            c.configure(format, surface, null, 0)
            c.start()
        } catch (e: RuntimeException) {
            c.release()
            throw e
        }
        codec = c
        broken = false
        running = true
        output = Thread({ drain(c) }, "hubbridge-video-out").also { it.start() }
    }

    private fun close() {
        running = false
        output?.join(500)
        output = null
        codec?.let {
            try {
                it.stop()
            } catch (e: IllegalStateException) {
                Log.w(TAG, "decoder stop: $e")
            }
            it.release()
        }
        codec = null
        queuedAt.clear()
    }

    /** [QUEUED], [NO_INPUT] (try the same frame again) or [ERROR] (send a keyframe). */
    fun queue(data: ByteArray, ptsUs: Long, config: Boolean, keyframe: Boolean): Int = synchronized(inputLock) {
        if (released) return ERROR
        try {
            if (broken) {
                close()
                open()
            }
            val c = codec ?: return ERROR
            val index = c.dequeueInputBuffer(INPUT_TIMEOUT_US)
            if (index < 0) return NO_INPUT
            val buffer = c.getInputBuffer(index) ?: return ERROR
            if (data.size > buffer.capacity()) {
                Log.w(TAG, "a frame of ${data.size} bytes does not fit the decoder's ${buffer.capacity()}")
                c.queueInputBuffer(index, 0, 0, ptsUs, 0)
                bad.incrementAndGet()
                return ERROR
            }
            buffer.clear()
            buffer.put(data)
            if (!config) queuedAt.put(ptsUs, System.nanoTime())
            if (keyframe) outputErrors.onKeyframeQueued(ptsUs)
            c.queueInputBuffer(index, 0, data.size, ptsUs, if (config) MediaCodec.BUFFER_FLAG_CODEC_CONFIG else 0)
            QUEUED
        } catch (e: RuntimeException) {
            // `MediaCodec.CodecException` and a codec in the wrong state.
            Log.e(TAG, "decoder input failed", e)
            broken = true
            bad.incrementAndGet()
            ERROR
        }
    }

    /** Take the pictures out; when several are ready only the newest is shown. */
    private fun drain(c: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        try {
            while (running) {
                var newest = c.dequeueOutputBuffer(info, OUTPUT_TIMEOUT_US)
                if (newest < 0) continue
                count(info.presentationTimeUs)
                while (running) {
                    val next = c.dequeueOutputBuffer(info, 0)
                    if (next == MediaCodec.INFO_TRY_AGAIN_LATER) break
                    if (next < 0) continue
                    c.releaseOutputBuffer(newest, false)
                    dropped.incrementAndGet()
                    newest = next
                    count(info.presentationTimeUs)
                }
                c.releaseOutputBuffer(newest, true)
            }
        } catch (e: RuntimeException) {
            if (running) {
                Log.e(TAG, "decoder output failed", e)
                broken = true
                bad.incrementAndGet()
                if (outputErrors.onError()) {
                    Log.e(TAG, "the keyframe after a decoder error failed too")
                    onFailed()
                } else {
                    onOutputError()
                }
            }
        }
    }

    private fun count(ptsUs: Long) {
        decoded.incrementAndGet()
        outputErrors.onPicture(ptsUs)
        val began = queuedAt.take(ptsUs) ?: return
        timeSumUs.addAndGet((System.nanoTime() - began) / 1000)
        timed.incrementAndGet()
    }

    /** The counters since this decoder started. */
    fun counters(): Counters = Counters(decoded.get(), dropped.get(), bad.get())

    /** Average input-to-output time over the last second; asked once a second. */
    @Synchronized
    fun decodeUs(): Long {
        val now = System.nanoTime()
        if (now - averagedAt >= 900_000_000L) {
            averagedAt = now
            val n = timed.getAndSet(0)
            val sum = timeSumUs.getAndSet(0)
            decodeUs = if (n > 0) sum / n else 0
        }
        return decodeUs
    }

    data class Counters(val decoded: Long = 0, val dropped: Long = 0, val bad: Long = 0) {
        operator fun plus(other: Counters) =
            Counters(decoded + other.decoded, dropped + other.dropped, bad + other.bad)
    }

    /** Let go of the surface. Calling it again does nothing. */
    fun release() {
        synchronized(inputLock) {
            if (released) return
            released = true
            close()
        }
    }

    companion object {
        private const val TAG = "HubBridge"
        const val QUEUED = 0
        const val NO_INPUT = 1
        const val ERROR = 2

        /** The counters for the host's `VideoStatus`. */
        fun statsJson(counters: Counters, decodeUs: Long): String = JSONObject()
            .put("decoded", counters.decoded)
            .put("dropped", counters.dropped)
            .put("bad", counters.bad)
            .put("decode_us", decodeUs)
            .toString()
        private const val INPUT_TIMEOUT_US = 500_000L
        private const val OUTPUT_TIMEOUT_US = 10_000L

        /** The phone's decoder for this size, or null when it has none. */
        fun find(width: Int, height: Int): String? = MediaCodecList(MediaCodecList.REGULAR_CODECS)
            .findDecoderForFormat(MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height))

        /**
         * A started decoder [name]; throws when the phone refuses. After an error when taking
         * pictures out it calls [onOutputError] (a keyframe is needed), or [onFailed] when
         * the keyframe did not help.
         */
        fun start(
            name: String,
            width: Int,
            height: Int,
            surface: Surface,
            onOutputError: () -> Unit,
            onFailed: () -> Unit,
        ): MirrorDecoder {
            val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height)
            format.setInteger(MediaFormat.KEY_PRIORITY, 0) // real time
            format.setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, width * height * 3 / 2)
            val lowLatency = Build.VERSION.SDK_INT >= 30 && supportsLowLatency(name)
            if (Build.VERSION.SDK_INT >= 30 && lowLatency) format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            val decoder = MirrorDecoder(name, format, surface, onOutputError, onFailed)
            decoder.open()
            Log.i(TAG, "decoder $name for ${width}x$height, low latency ${if (lowLatency) "on" else "off"}")
            return decoder
        }

        @android.annotation.TargetApi(30)
        private fun supportsLowLatency(name: String): Boolean {
            val info = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.firstOrNull { it.name == name }
                ?: return false
            return info.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
                .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
        }
    }
}
