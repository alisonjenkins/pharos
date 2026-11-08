package com.alisonjenkins.pharos.tvnative.video

import android.app.Activity
import android.graphics.Color
import android.util.Log
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.widget.FrameLayout

/**
 * Video hole probe (spec 032 slice 0): a SurfaceView in the given rect,
 * filled with one colour. A SurfaceView sits below its window by default,
 * so it shows only where the Blitz UI above it is transparent. A player
 * would hand this surface to MediaCodec or ExoPlayer instead of drawing.
 */
class VideoHole(private val activity: Activity) {
    fun show(left: Int, top: Int, width: Int, height: Int) {
        activity.runOnUiThread {
            val view = SurfaceView(activity)
            view.holder.addCallback(object : SurfaceHolder.Callback {
                override fun surfaceCreated(holder: SurfaceHolder) = fill(holder)
                override fun surfaceChanged(holder: SurfaceHolder, format: Int, w: Int, h: Int) {
                    Log.i(TAG, "surfaceChanged ${w}x$h format=$format")
                    fill(holder)
                }
                override fun surfaceDestroyed(holder: SurfaceHolder) {
                    Log.i(TAG, "surfaceDestroyed")
                }
            })
            val params = FrameLayout.LayoutParams(width, height).apply {
                leftMargin = left
                topMargin = top
            }
            activity.addContentView(view, params)
            Log.i(TAG, "SurfaceView added at $left,$top ${width}x$height")
        }
    }

    private fun fill(holder: SurfaceHolder) {
        val canvas = holder.lockCanvas() ?: run {
            Log.w(TAG, "lockCanvas returned null")
            return
        }
        canvas.drawColor(Color.rgb(230, 25, 25))
        holder.unlockCanvasAndPost(canvas)
    }

    private companion object {
        const val TAG = "pharos-bench"
    }
}
