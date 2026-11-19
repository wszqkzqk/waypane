package org.waypane.app

import android.content.Context
import android.os.Handler
import android.os.HandlerThread
import android.os.Process
import android.text.InputType
import android.util.Log
import android.view.Choreographer
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewConfiguration
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import kotlin.math.abs
import kotlin.math.hypot
import kotlin.math.max
import kotlin.math.min

class RemoteSurfaceView(
    context: Context,
    private val key: SurfaceKey,
    contentAspectRatio: Float,
    private val nativeHandle: () -> Long,
    private val onUnsupportedText: (String) -> Unit = {},
    private val onError: (String) -> Unit = {},
    private val onViewSizeChanged: (Int, Int) -> Unit = { _, _ -> },
) : SurfaceView(context), SurfaceHolder.Callback, Choreographer.FrameCallback {

    @Volatile
    private var contentAspectRatio =
        if (contentAspectRatio.isFinite() && contentAspectRatio > 0f) contentAspectRatio else 1f

    @Volatile
    private var attached = false
    private var waitingForFrame = false
    private var zoom = MIN_VIEWPORT_ZOOM
    private var panX = 0f
    private var panY = 0f
    private var viewportGesture = false
    private var gestureStartedAt = 0L
    private var gestureStartSpan = 1f
    private var gestureStartCenterX = 0.5f
    private var gestureStartCenterY = 0.5f
    private var gestureLastCenterX = 0.5f
    private var gestureLastCenterY = 0.5f
    private var gestureStartZoom = MIN_VIEWPORT_ZOOM
    private var gestureStartPanX = 0f
    private var gestureStartPanY = 0f
    private var gestureTravel = 0f
    private var gestureSpanDelta = 0f
    private var gestureScroll = false
    private var gestureEnding = false
    private var scrollPosted = false
    private var pendingScrollX = 0f
    private var pendingScrollY = 0f
    private var pendingHorizontal = 0f
    private var pendingVertical = 0f
    private val scrollDispatcher = Runnable { dispatchPendingScroll() }
    private val activePointers = mutableSetOf<Int>()
    private var directTouch = false
    private var pointerContact = false
    private var pointerId = MotionEvent.INVALID_POINTER_ID
    private var pointerStartX = 0f
    private var pointerStartY = 0f
    private var pointerX = 0f
    private var pointerY = 0f
    private var pointerDragging = false
    private var longPressCompleted = false
    private val longPressRightClick = Runnable { completeLongPressRightClick() }
    private val touchSlopPx = ViewConfiguration.get(context).scaledTouchSlop * 2f
    private val longPressTimeoutMs = ViewConfiguration.getLongPressTimeout().toLong()
    private val inputMethodManager = context.getSystemService(InputMethodManager::class.java)

    private data class GestureGeometry(
        val x: Float,
        val y: Float,
        val spanPx: Float,
    )

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

    override fun onSizeChanged(width: Int, height: Int, oldWidth: Int, oldHeight: Int) {
        super.onSizeChanged(width, height, oldWidth, oldHeight)
        if (width > 0 && height > 0) {
            onViewSizeChanged(width, height)
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
        }
    }

    override fun onDetachedFromWindow() {
        clearPendingScroll()
        val handle = handle()
        if (handle != 0L) cancelRemoteInput(handle)
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
                    DIRTY_WAIT_MS,
                )
            }.onFailure { error ->
                reportRenderError("Failed to wait for Wayland frames", error)
            }.getOrNull() ?: return@post
            waitingForFrame = false
            if (version > 0L && attached && handle == handle()) {
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
        if (handleViewportGesture(event)) return true
        return handleRemoteTouch(event, handle)
    }

    private fun handleViewportGesture(event: MotionEvent): Boolean {
        val handle = handle()
        if (handle == 0L) return false
        when (event.actionMasked) {
            MotionEvent.ACTION_POINTER_DOWN -> {
                if (event.pointerCount >= 2) {
                    startViewportGesture(event, handle)
                    return true
                }
            }
            MotionEvent.ACTION_MOVE -> {
                if (viewportGesture) {
                    updateViewportGesture(event)
                    return true
                }
            }
            MotionEvent.ACTION_POINTER_UP -> {
                if (viewportGesture) {
                    if (event.pointerCount == 2) {
                        gestureEnding = true
                        finishTwoFingerTapIfEligible(event)
                    }
                    return true
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                if (viewportGesture) {
                    if (event.actionMasked == MotionEvent.ACTION_CANCEL) clearPendingScroll()
                    viewportGesture = false
                    gestureEnding = false
                    return true
                }
            }
        }
        return viewportGesture
    }

    private fun startViewportGesture(event: MotionEvent, handle: Long) {
        clearPendingScroll()
        if (!cancelRemoteInput(handle)) {
            reportRenderError(
                "Failed to cancel remote input before viewport gesture",
                IllegalStateException("Input queue is closed or full"),
            )
            return
        }
        viewportGesture = true
        val geometry = gestureGeometry(event)
        gestureStartedAt = event.eventTime
        gestureStartSpan = geometry.spanPx
        gestureStartCenterX = geometry.x
        gestureStartCenterY = geometry.y
        gestureLastCenterX = geometry.x
        gestureLastCenterY = geometry.y
        gestureStartZoom = zoom
        gestureStartPanX = panX
        gestureStartPanY = panY
        gestureTravel = 0f
        gestureSpanDelta = 0f
        gestureScroll = false
        gestureEnding = false
    }

    private fun updateViewportGesture(event: MotionEvent) {
        if (gestureEnding) return
        val geometry = gestureGeometry(event)
        gestureTravel = max(
            gestureTravel,
            hypot(
                (geometry.x - gestureStartCenterX) * width,
                (geometry.y - gestureStartCenterY) * height,
            ),
        )
        gestureSpanDelta = max(gestureSpanDelta, abs(geometry.spanPx - gestureStartSpan))
        val (limitX, limitY) = panLimits(gestureStartZoom)
        if (gestureSpanDelta <= touchSlopPx &&
            limitX == 0f && limitY == 0f &&
            gestureTravel > touchSlopPx
        ) {
            gestureScroll = true
        }

        if (gestureScroll && gestureSpanDelta <= touchSlopPx) {
            queueRemoteScroll(geometry)
        } else {
            if (gestureScroll) clearPendingScroll()
            gestureScroll = false
            val targetZoom = gestureStartZoom * (geometry.spanPx / gestureStartSpan)
            val ratio = targetZoom / gestureStartZoom
            applyViewport(
                targetZoom,
                transformedPan(gestureStartPanX, gestureStartCenterX, geometry.x, ratio),
                transformedPan(gestureStartPanY, gestureStartCenterY, geometry.y, ratio),
            )
        }
        gestureLastCenterX = geometry.x
        gestureLastCenterY = geometry.y
    }

    private fun finishTwoFingerTapIfEligible(event: MotionEvent) {
        if (event.eventTime - gestureStartedAt > TWO_FINGER_TAP_TIMEOUT_MS ||
            gestureTravel > touchSlopPx ||
            gestureSpanDelta > touchSlopPx
        ) {
            return
        }
        if (zoom > MIN_VIEWPORT_ZOOM + VIEWPORT_EPSILON) {
            applyViewport(MIN_VIEWPORT_ZOOM, 0f, 0f)
            return
        }
        val geometry = gestureGeometry(event)
        val ratio = DOUBLE_TAP_ZOOM / zoom
        applyViewport(
            DOUBLE_TAP_ZOOM,
            transformedPan(panX, geometry.x, geometry.x, ratio),
            transformedPan(panY, geometry.y, geometry.y, ratio),
        )
    }

    private fun transformedPan(startPan: Float, startCenter: Float, center: Float, ratio: Float): Float =
        ratio * (startPan - startCenter + 0.5f) + center - 0.5f

    private fun queueRemoteScroll(geometry: GestureGeometry) {
        pendingScrollX = geometry.x
        pendingScrollY = geometry.y
        pendingHorizontal = (pendingHorizontal +
            (gestureLastCenterX - geometry.x) * VIEWPORT_SCROLL_SCALE)
            .coerceIn(-MAX_PENDING_SCROLL, MAX_PENDING_SCROLL)
        pendingVertical = (pendingVertical +
            (gestureLastCenterY - geometry.y) * VIEWPORT_SCROLL_SCALE)
            .coerceIn(-MAX_PENDING_SCROLL, MAX_PENDING_SCROLL)
        schedulePendingScroll()
    }

    private fun schedulePendingScroll() {
        if (scrollPosted) return
        scrollPosted = true
        postOnAnimation(scrollDispatcher)
    }

    private fun dispatchPendingScroll() {
        scrollPosted = false
        if (pendingHorizontal == 0f && pendingVertical == 0f) return
        val handle = handle()
        if (handle == 0L) {
            clearPendingScroll()
            return
        }
        val accepted = runCatching {
            NativeBridge.nativeScroll(
                handle,
                key.session,
                key.window,
                pendingScrollX,
                pendingScrollY,
                pendingHorizontal,
                pendingVertical,
            )
        }.getOrElse { error ->
            clearPendingScroll()
            reportRenderError("Failed to forward Wayland scroll", error)
            return
        }
        if (accepted) {
            pendingHorizontal = 0f
            pendingVertical = 0f
        } else if (viewportGesture) {
            schedulePendingScroll()
        } else {
            pendingHorizontal = 0f
            pendingVertical = 0f
        }
    }

    private fun clearPendingScroll() {
        removeCallbacks(scrollDispatcher)
        scrollPosted = false
        pendingHorizontal = 0f
        pendingVertical = 0f
    }

    private fun gestureGeometry(event: MotionEvent): GestureGeometry {
        var minX = Float.POSITIVE_INFINITY
        var maxX = Float.NEGATIVE_INFINITY
        var minY = Float.POSITIVE_INFINITY
        var maxY = Float.NEGATIVE_INFINITY
        var sumX = 0f
        var sumY = 0f
        for (index in 0 until event.pointerCount) {
            val x = event.getX(index)
            val y = event.getY(index)
            minX = min(minX, x)
            maxX = max(maxX, x)
            minY = min(minY, y)
            maxY = max(maxY, y)
            sumX += x
            sumY += y
        }
        val count = event.pointerCount.coerceAtLeast(1)
        return GestureGeometry(
            sumX / count / width,
            sumY / count / height,
            hypot(maxX - minX, maxY - minY).coerceAtLeast(1f),
        )
    }

    fun updateContentAspectRatio(ratio: Float) {
        contentAspectRatio = if (ratio.isFinite() && ratio > 0f) ratio else 1f
        applyViewport(zoom, panX, panY)
    }

    fun resetViewport() {
        applyViewport(MIN_VIEWPORT_ZOOM, 0f, 0f)
    }

    fun setDirectTouch(enabled: Boolean) {
        if (directTouch == enabled) return
        val handle = handle()
        if (handle != 0L && !cancelRemoteInput(handle)) {
            reportRenderError(
                "Failed to cancel remote input before changing input mode",
                IllegalStateException("Input queue is closed or full"),
            )
        }
        directTouch = enabled
    }

    fun showKeyboard() {
        post {
            if (!isAttachedToWindow) return@post
            requestFocus()
            val accepted =
                inputMethodManager?.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT) == true
            if (!accepted) Log.w(TAG, "System input method rejected showSoftInput")
        }
    }

    private fun drawSize(zoom: Float): Pair<Float, Float> {
        val viewAspect = width.toFloat() / height.toFloat()
        val bufferAspect = contentAspectRatio.coerceAtLeast(0.0001f)
        return if (bufferAspect >= viewAspect) {
            zoom to zoom * viewAspect / bufferAspect
        } else {
            zoom * bufferAspect / viewAspect to zoom
        }
    }

    private fun isInsideContent(x: Float, y: Float): Boolean {
        if (width <= 0 || height <= 0) return false
        val (drawWidth, drawHeight) = drawSize(zoom)
        val left = (1f - drawWidth) * 0.5f + panX
        val top = (1f - drawHeight) * 0.5f + panY
        return (x - left) / drawWidth in 0f..1f && (y - top) / drawHeight in 0f..1f
    }

    private fun panLimits(zoom: Float): Pair<Float, Float> {
        if (width <= 0 || height <= 0) return 0f to 0f
        val (drawWidth, drawHeight) = drawSize(zoom)
        return ((drawWidth - 1f).coerceAtLeast(0f) * 0.5f) to
            ((drawHeight - 1f).coerceAtLeast(0f) * 0.5f)
    }

    private fun applyViewport(targetZoom: Float, targetPanX: Float, targetPanY: Float) {
        if (!targetZoom.isFinite() || !targetPanX.isFinite() || !targetPanY.isFinite()) return
        val nextZoom = targetZoom.coerceIn(MIN_VIEWPORT_ZOOM, MAX_VIEWPORT_ZOOM)
        val (limitX, limitY) = panLimits(nextZoom)
        val nextPanX = targetPanX.coerceIn(-limitX, limitX)
        val nextPanY = targetPanY.coerceIn(-limitY, limitY)
        if (abs(zoom - nextZoom) <= VIEWPORT_EPSILON &&
            abs(panX - nextPanX) <= VIEWPORT_EPSILON &&
            abs(panY - nextPanY) <= VIEWPORT_EPSILON
        ) {
            return
        }
        zoom = nextZoom
        panX = nextPanX
        panY = nextPanY
        sendViewport()
    }

    private fun sendViewport() {
        val handle = handle()
        if (handle == 0L) return
        runCatching {
            NativeBridge.nativeSetViewport(
                handle,
                key.session,
                key.window,
                zoom,
                panX,
                panY,
            )
        }.onFailure { error ->
            reportRenderError("Failed to update viewport", error)
            return
        }
        renderHandler.post { requestRenderIfDirty() }
    }

    private fun handleRemoteTouch(event: MotionEvent, handle: Long): Boolean {
        return if (directTouch) handleDirectTouch(event, handle) else handlePointerTouch(event, handle)
    }

    private fun handlePointerTouch(event: MotionEvent, handle: Long): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                if (!cancelRemoteInput(handle)) {
                    reportRenderError(
                        "Failed to reset remote input before pointer gesture",
                        IllegalStateException("Input queue is closed or full"),
                    )
                    return true
                }
                requestFocus()
                val index = event.actionIndex
                pointerId = event.getPointerId(index)
                pointerStartX = event.getX(index)
                pointerStartY = event.getY(index)
                pointerX = pointerStartX / width
                pointerY = pointerStartY / height
                pointerContact = isInsideContent(pointerX, pointerY)
                pointerDragging = false
                longPressCompleted = false
                if (pointerContact) {
                    sendRemotePointer(handle, pointerX, pointerY, NO_BUTTON, false)
                    postDelayed(longPressRightClick, longPressTimeoutMs)
                }
            }
            MotionEvent.ACTION_MOVE -> {
                if (!pointerContact) return true
                val index = event.findPointerIndex(pointerId)
                if (index < 0 || longPressCompleted) return true
                pointerX = event.getX(index) / width
                pointerY = event.getY(index) / height
                if (!pointerDragging &&
                    hypot(event.getX(index) - pointerStartX, event.getY(index) - pointerStartY) > touchSlopPx
                ) {
                    pointerDragging = true
                    removeCallbacks(longPressRightClick)
                    val startX = pointerStartX / width
                    val startY = pointerStartY / height
                    sendRemotePointer(handle, startX, startY, NO_BUTTON, false)
                    sendRemotePointer(handle, startX, startY, LINUX_BUTTON_LEFT, true)
                }
                sendRemotePointer(handle, pointerX, pointerY, NO_BUTTON, false)
            }
            MotionEvent.ACTION_UP -> {
                val index = event.actionIndex
                if (pointerContact && event.getPointerId(index) == pointerId && !longPressCompleted) {
                    pointerX = event.getX(index) / width
                    pointerY = event.getY(index) / height
                    if (pointerDragging) {
                        sendRemotePointer(handle, pointerX, pointerY, LINUX_BUTTON_LEFT, false)
                    } else {
                        sendRemotePointer(handle, pointerX, pointerY, LINUX_BUTTON_LEFT, true)
                        sendRemotePointer(handle, pointerX, pointerY, LINUX_BUTTON_LEFT, false)
                    }
                }
                resetPointerGesture()
            }
            MotionEvent.ACTION_CANCEL -> {
                if (!cancelRemoteInput(handle)) {
                    Log.d(TAG, "Pointer cancellation skipped because the input queue is closed or full")
                }
            }
            else -> return super.onTouchEvent(event)
        }
        return true
    }

    private fun completeLongPressRightClick() {
        val handle = handle()
        if (!pointerContact || pointerDragging || handle == 0L) return
        longPressCompleted = true
        sendRemotePointer(handle, pointerX, pointerY, LINUX_BUTTON_RIGHT, true)
        sendRemotePointer(handle, pointerX, pointerY, LINUX_BUTTON_RIGHT, false)
    }

    private fun cancelRemoteInput(handle: Long): Boolean {
        removeCallbacks(longPressRightClick)
        var accepted = true
        if (activePointers.isNotEmpty() && handle != 0L) {
            accepted = runCatching {
                NativeBridge.nativeTouchCancel(handle, key.session, key.window)
            }.getOrDefault(false)
        }
        activePointers.clear()
        if (pointerDragging && handle != 0L) {
            accepted = sendRemotePointer(handle, pointerX, pointerY, LINUX_BUTTON_LEFT, false) && accepted
        }
        resetPointerGesture()
        return accepted
    }

    private fun resetPointerGesture() {
        removeCallbacks(longPressRightClick)
        pointerContact = false
        pointerId = MotionEvent.INVALID_POINTER_ID
        pointerDragging = false
        longPressCompleted = false
    }

    private fun sendRemotePointer(
        handle: Long,
        x: Float,
        y: Float,
        button: Int,
        pressed: Boolean,
    ): Boolean = runCatching {
        NativeBridge.nativePointer(handle, key.session, key.window, x, y, button, pressed)
    }.onFailure { error ->
        reportRenderError("Failed to forward Wayland pointer input", error)
    }.getOrDefault(false)

    private fun handleDirectTouch(event: MotionEvent, handle: Long): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                requestFocus()
                val index = event.actionIndex
                val id = event.getPointerId(index)
                activePointers += id
                sendRemoteTouch(handle, id, TOUCH_DOWN, event.getX(index) / width, event.getY(index) / height)
            }
            MotionEvent.ACTION_MOVE -> {
                for (index in 0 until event.pointerCount) {
                    val id = event.getPointerId(index)
                    if (id in activePointers) {
                        sendRemoteTouch(
                            handle,
                            id,
                            TOUCH_MOTION,
                            event.getX(index) / width,
                            event.getY(index) / height,
                        )
                    }
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
                val index = event.actionIndex
                val id = event.getPointerId(index)
                activePointers -= id
                sendRemoteTouch(
                    handle,
                    id,
                    TOUCH_UP,
                    event.getX(index) / width,
                    event.getY(index) / height,
                )
            }
            MotionEvent.ACTION_CANCEL -> {
                if (!cancelRemoteInput(handle)) {
                    Log.d(TAG, "Touch cancellation skipped because the input queue is closed or full")
                }
            }
            else -> return super.onTouchEvent(event)
        }
        return true
    }

    private fun sendRemoteTouch(handle: Long, id: Int, action: Int, x: Float, y: Float) {
        runCatching {
            NativeBridge.nativeTouch(handle, key.session, key.window, id, action, x, y)
        }.onFailure { error ->
            reportRenderError("Failed to forward Wayland touch input", error)
        }
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
        private const val MIN_VIEWPORT_ZOOM = 1f
        private const val MAX_VIEWPORT_ZOOM = 8f
        private const val DOUBLE_TAP_ZOOM = 2.5f
        private const val TWO_FINGER_TAP_TIMEOUT_MS = 250L
        private const val VIEWPORT_SCROLL_SCALE = 80f
        private const val MAX_PENDING_SCROLL = 1_000f
        private const val VIEWPORT_EPSILON = 1e-5f
        private const val TOUCH_DOWN = 0
        private const val TOUCH_MOTION = 1
        private const val TOUCH_UP = 2
        private const val NO_BUTTON = -1
        private const val LINUX_BUTTON_LEFT = 0x110
        private const val LINUX_BUTTON_RIGHT = 0x111
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
