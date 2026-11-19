package org.waypane.app

import android.content.Context
import android.os.Handler
import android.os.HandlerThread
import android.os.Process
import android.util.Log
import android.view.Choreographer
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.text.InputType

class RemoteSurfaceView(
    context: Context,
    private val key: SurfaceKey,
    private val nativeHandle: () -> Long,
    private val onUnsupportedText: (String) -> Unit = {},
    private val onError: (String) -> Unit = {},
) : SurfaceView(context), SurfaceHolder.Callback, Choreographer.FrameCallback {

    @Volatile
    private var attached = false
    private var waitingForFrame = false
    private var seenVersion = 0L
    private val activePointers = mutableSetOf<Int>()
    private val inputMethodManager = context.getSystemService(InputMethodManager::class.java)

    init {
        holder.addCallback(this)
        isFocusable = true
        isFocusableInTouchMode = true
    }

    private fun handle() = nativeHandle()

    private fun reportRenderError(action: String, error: Throwable) {
        attached = false
        waitingForFrame = false
        val message = "$action: ${error.message ?: error.toString()}"
        Log.w(TAG, message, error)
        post { onError(message) }
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        seenVersion = 0L
        renderHandler.post {
            val handle = handle()
            if (handle == 0L) return@post
            runCatching {
                attached = NativeBridge.nativeAttachSurface(
                    handle,
                    key.session,
                    key.window,
                    holder.surface,
                    width.coerceAtLeast(1),
                    height.coerceAtLeast(1),
                )
                if (!attached) error("nativeAttachSurface returned false")
                NativeBridge.nativeResizeSurface(handle, key.session, key.window, width, height)
            }.onFailure { error ->
                reportRenderError("Failed to initialize Android GPU surface", error)
            }
            if (attached) scheduleDirtyWait()
        }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        renderHandler.post {
            val handle = handle()
            if (attached && handle != 0L) {
                runCatching {
                    NativeBridge.nativeResizeSurface(handle, key.session, key.window, width, height)
                }.onFailure { error ->
                    reportRenderError("Failed to resize Android GPU surface", error)
                }
            }
            if (attached) requestRenderIfDirty()
        }
    }

    override fun onWindowVisibilityChanged(visibility: Int) {
        super.onWindowVisibilityChanged(visibility)
        if (visibility == View.VISIBLE && attached) {
            renderHandler.post { requestRenderIfDirty() }
        }
    }

    private fun wakeNativeSurface(handle: Long) {
        if (handle == 0L) return
        runCatching {
            NativeBridge.nativeWakeSurface(handle, key.session, key.window)
        }.onFailure { error ->
            Log.d(TAG, "Surface wakeup skipped after native handle closed", error)
        }
    }

    private fun detachNativeSurface(handle: Long) {
        if (handle == 0L) return
        runCatching {
            NativeBridge.nativeDetachSurface(handle, key.session, key.window)
        }.onFailure { error ->
            if (handle() != 0L) {
                reportRenderError("Failed to release Android GPU surface", error)
            } else {
                Log.d(TAG, "Surface detach skipped after native handle closed", error)
            }
        }
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        attached = false
        wakeNativeSurface(handle())
        val handle = handle()
        renderHandler.post {
            waitingForFrame = false
            detachNativeSurface(handle)
            seenVersion = 0L
        }
    }

    override fun onDetachedFromWindow() {
        val handle = handle()
        if (attached) wakeNativeSurface(handle)
        attached = false
        holder.removeCallback(this)
        renderHandler.post {
            waitingForFrame = false
            detachNativeSurface(handle)
        }
        super.onDetachedFromWindow()
    }

    private fun scheduleDirtyWait() {
        if (waitingForFrame || !attached) return
        waitingForFrame = true
        renderHandler.post {
            if (!attached || handle() == 0L) {
                waitingForFrame = false
                return@post
            }
            val handle = handle()
            val version = runCatching {
                NativeBridge.nativeWaitDirty(
                    handle,
                    key.session,
                    key.window,
                    seenVersion,
                    DIRTY_WAIT_MS,
                )
            }.onFailure { error ->
                reportRenderError("Failed to wait for Wayland frames", error)
            }.getOrNull() ?: return@post
            waitingForFrame = false
            if (version > 0L && attached && handle == handle()) {
                seenVersion = version
                Choreographer.getInstance().postFrameCallback(this@RemoteSurfaceView)
            } else if (attached && handle == handle()) {
                scheduleDirtyWait()
            }
        }
    }

    private fun requestRenderIfDirty() {
        if (!attached) return
        val handle = handle()
        if (handle == 0L) return
        val dirty = runCatching {
            NativeBridge.nativeIsDirty(handle, key.session, key.window)
        }.onFailure { error ->
            reportRenderError("Failed to inspect Wayland surface state", error)
        }.getOrNull() ?: return
        if (dirty) Choreographer.getInstance().postFrameCallback(this)
    }

    override fun doFrame(frameTimeNanos: Long) {
        if (!attached) return
        val handle = handle()
        if (handle == 0L) return
        val outcome = runCatching {
            NativeBridge.nativeRender(handle, key.session, key.window)
        }.onFailure { error ->
            reportRenderError("Failed to render Android GPU surface", error)
            return
        }.getOrNull() ?: return
        when (outcome) {
            RENDER_IDLE, RENDER_PRESENTED -> scheduleDirtyWait()
            RENDER_RETRY -> Choreographer.getInstance().postFrameCallback(this)
            RENDER_OCCLUDED -> Unit
            else -> reportRenderError(
                "Unknown native render outcome $outcome",
                IllegalStateException("nativeRender returned $outcome"),
            )
        }
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        if (width <= 0 || height <= 0) return false
        val handle = handle()
        if (handle == 0L) return false
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                requestFocus()
                val index = event.actionIndex
                val id = event.getPointerId(index)
                activePointers += id
                NativeBridge.nativeTouch(
                    handle,
                    key.session,
                    key.window,
                    id,
                    TOUCH_DOWN,
                    event.getX(index) / width,
                    event.getY(index) / height,
                )
            }
            MotionEvent.ACTION_MOVE -> {
                for (index in 0 until event.pointerCount) {
                    val id = event.getPointerId(index)
                    if (id in activePointers) {
                        NativeBridge.nativeTouch(
                            handle,
                            key.session,
                            key.window,
                            id,
                            TOUCH_MOTION,
                            event.getX(index) / width,
                            event.getY(index) / height,
                        )
                    }
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
                val id = event.getPointerId(event.actionIndex)
                activePointers -= id
                NativeBridge.nativeTouch(
                    handle,
                    key.session,
                    key.window,
                    id,
                    TOUCH_UP,
                    event.getX(event.actionIndex) / width,
                    event.getY(event.actionIndex) / height,
                )
                if (event.getToolType(event.actionIndex) != MotionEvent.TOOL_TYPE_MOUSE) {
                    inputMethodManager?.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
                }
            }
            MotionEvent.ACTION_CANCEL -> {
                activePointers.clear()
                NativeBridge.nativeTouchCancel(handle, key.session, key.window)
            }
            else -> return super.onTouchEvent(event)
        }
        return true
    }

    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        if (event.source and InputDevice.SOURCE_CLASS_POINTER == 0 || width <= 0 || height <= 0) {
            return super.onGenericMotionEvent(event)
        }
        val handle = handle()
        if (handle == 0L) return false
        val x = event.x / width
        val y = event.y / height
        when (event.action) {
            MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_MOVE -> {
                NativeBridge.nativePointer(handle, key.session, key.window, x, y, NO_BUTTON, false)
            }
            MotionEvent.ACTION_BUTTON_PRESS, MotionEvent.ACTION_BUTTON_RELEASE -> {
                val button = linuxButton(event.actionButton)
                if (button == NO_BUTTON) return super.onGenericMotionEvent(event)
                NativeBridge.nativePointer(
                    handle,
                    key.session,
                    key.window,
                    x,
                    y,
                    button,
                    event.action == MotionEvent.ACTION_BUTTON_PRESS,
                )
            }
            MotionEvent.ACTION_SCROLL -> {
                NativeBridge.nativeScroll(
                    handle,
                    key.session,
                    key.window,
                    x,
                    y,
                    event.getAxisValue(MotionEvent.AXIS_HSCROLL) * 10.0f,
                    event.getAxisValue(MotionEvent.AXIS_VSCROLL) * 10.0f,
                )
            }
            else -> return super.onGenericMotionEvent(event)
        }
        return true
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        val handle = handle()
        return if (handle != 0L && NativeBridge.nativeKey(handle, key.session, key.window, keyCode, true)) {
            true
        } else {
            super.onKeyDown(keyCode, event)
        }
    }

    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean {
        val handle = handle()
        return if (handle != 0L && NativeBridge.nativeKey(handle, key.session, key.window, keyCode, false)) {
            true
        } else {
            super.onKeyUp(keyCode, event)
        }
    }

    override fun onCheckIsTextEditor() = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or
            InputType.TYPE_TEXT_FLAG_MULTI_LINE or
            InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_ACTION_DONE or
            EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN
        outAttrs.packageName = context.packageName
        outAttrs.fieldId = id
        return RemoteInputConnection(this, true)
    }

    private inner class RemoteInputConnection(view: View, fullEditor: Boolean) : BaseInputConnection(view, fullEditor) {
        override fun sendKeyEvent(event: KeyEvent): Boolean {
            val handle = handle()
            return handle != 0L && NativeBridge.nativeKey(
                handle,
                key.session,
                key.window,
                event.keyCode,
                event.action == KeyEvent.ACTION_DOWN,
            )
        }

        override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
            val handle = handle()
            if (handle == 0L) return false
            return runCatching {
                NativeBridge.nativeCommitText(handle, key.session, key.window, text.toString()) &&
                    super.commitText(text, newCursorPosition)
            }.getOrElse { error ->
                onUnsupportedText(error.message ?: text.toString())
                false
            }
        }

        override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
            val handle = handle()
            if (handle == 0L) return false
            return runCatching {
                NativeBridge.nativeSetPreeditText(
                    handle,
                    key.session,
                    key.window,
                    text.toString(),
                    0,
                    text.length,
                ) && super.setComposingText(text, newCursorPosition)
            }.getOrElse { error ->
                onUnsupportedText(error.message ?: text.toString())
                false
            }
        }

        override fun finishComposingText(): Boolean {
            val handle = handle()
            if (handle != 0L) {
                NativeBridge.nativeSetPreeditText(handle, key.session, key.window, "", -1, -1)
            }
            return super.finishComposingText()
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            val handle = handle()
            if (handle == 0L) return false
            return NativeBridge.nativeDeleteSurroundingText(
                handle,
                key.session,
                key.window,
                beforeLength,
                afterLength,
            ) && super.deleteSurroundingText(beforeLength, afterLength)
        }

        override fun performEditorAction(actionCode: Int): Boolean {
            sendKey(KeyEvent.KEYCODE_ENTER)
            return true
        }

        private fun sendKey(keyCode: Int) {
            sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, keyCode))
            sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, keyCode))
        }
    }

    companion object {
        private const val TAG = "WaypaneRender"
        private const val RENDER_IDLE = 0
        private const val RENDER_PRESENTED = 1
        private const val RENDER_RETRY = 2
        private const val RENDER_OCCLUDED = 3
        private const val TOUCH_DOWN = 0
        private const val TOUCH_MOTION = 1
        private const val TOUCH_UP = 2
        private const val NO_BUTTON = -1
        private const val DIRTY_WAIT_MS = 1_000

        private val renderThread = HandlerThread("WaypaneRender", Process.THREAD_PRIORITY_DISPLAY).apply {
            start()
        }
        private val renderHandler = Handler(renderThread.looper)

        private fun linuxButton(androidButton: Int): Int = when (androidButton) {
            MotionEvent.BUTTON_PRIMARY -> 0x110
            MotionEvent.BUTTON_SECONDARY -> 0x111
            MotionEvent.BUTTON_TERTIARY -> 0x112
            MotionEvent.BUTTON_BACK -> 0x113
            MotionEvent.BUTTON_FORWARD -> 0x114
            else -> NO_BUTTON
        }
    }
}
