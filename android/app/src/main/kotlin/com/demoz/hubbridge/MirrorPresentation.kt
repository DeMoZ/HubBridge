package com.demoz.hubbridge

import android.app.Presentation
import android.content.Context
import android.graphics.Color
import android.os.Bundle
import android.view.Display
import android.view.Gravity
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.WindowManager
import android.widget.FrameLayout

/**
 * The picture on the monitor: a black window on the phone's external display, owned by the
 * service, with a surface of the stream's shape in the middle. It needs Samsung DeX off.
 * Main thread only.
 */
class MirrorPresentation(
    context: Context,
    display: Display,
    private val videoWidth: Int,
    private val videoHeight: Int,
    private val onSurfaceReady: (Surface) -> Unit,
    private val onSurfaceLost: () -> Unit,
) : Presentation(context, display) {

    private var closing = false
    private var lost = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        val video = AspectSurfaceView(context, videoWidth, videoHeight)
        // The decoder writes pictures of the stream size; the display hardware scales them.
        video.holder.setFixedSize(videoWidth, videoHeight)
        video.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) = onSurfaceReady(holder.surface)
            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) = Unit
            // The decoder has to let go of the surface before this returns.
            override fun surfaceDestroyed(holder: SurfaceHolder) = reportLost()
        })
        val frame = FrameLayout(context).apply {
            setBackgroundColor(Color.BLACK)
            addView(video, FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT, Gravity.CENTER))
        }
        setContentView(frame)
        // Android cancels it when the display goes away or its size changes; [MirrorOutput]
        // opens a new one for a display that is still there.
        setOnDismissListener { reportLost() }
    }

    /** Remove the window on purpose. */
    fun close() {
        closing = true
        dismiss()
    }

    private fun reportLost() {
        if (closing || lost) return
        lost = true
        onSurfaceLost()
    }
}

/** The largest view of the video's shape that fits the space given; also after a mode change. */
private class AspectSurfaceView(context: Context, private val videoWidth: Int, private val videoHeight: Int) :
    SurfaceView(context) {

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val maxWidth = View.MeasureSpec.getSize(widthMeasureSpec)
        val maxHeight = View.MeasureSpec.getSize(heightMeasureSpec)
        if (maxWidth.toLong() * videoHeight <= maxHeight.toLong() * videoWidth) {
            setMeasuredDimension(maxWidth, (maxWidth.toLong() * videoHeight / videoWidth).toInt())
        } else {
            setMeasuredDimension((maxHeight.toLong() * videoWidth / videoHeight).toInt(), maxHeight)
        }
    }
}
