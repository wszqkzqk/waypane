package org.waypane.app

import android.view.Surface

object NativeBridge {
    init {
        System.loadLibrary("waypane_android")
    }

    external fun nativeConnect(
        host: String,
        port: Int,
        user: String,
        identity: String,
        passphrase: String,
        password: String,
        knownHosts: String,
        remoteSocket: String,
        compression: String,
        command: String,
        waylandScale: Int,
    ): Long

    external fun nativeResolveHostKey(handle: Long, trusted: Boolean): Boolean
    external fun nativeNextEvent(handle: Long): String?

    external fun nativeAttachSurface(
        handle: Long,
        session: Long,
        window: Int,
        surface: Surface,
        width: Int,
        height: Int,
    ): Boolean

    external fun nativeResizeSurface(handle: Long, session: Long, window: Int, width: Int, height: Int)
    external fun nativeSetViewport(handle: Long, session: Long, window: Int, zoom: Float, panX: Float, panY: Float)
    external fun nativeSetOutputScale(handle: Long, scale: Int): Boolean
    external fun nativeRender(handle: Long, session: Long, window: Int): Int
    external fun nativeDetachSurface(handle: Long, session: Long, window: Int)
    external fun nativeIsDirty(handle: Long, session: Long, window: Int): Boolean
    external fun nativeWaitDirty(handle: Long, session: Long, window: Int, timeoutMs: Int): Long
    external fun nativeWakeSurface(handle: Long, session: Long, window: Int)
    external fun nativeTouch(handle: Long, session: Long, window: Int, pointer: Int, action: Int, x: Float, y: Float): Boolean
    external fun nativeTouchCancel(handle: Long, session: Long, window: Int): Boolean
    external fun nativePointer(handle: Long, session: Long, window: Int, x: Float, y: Float, button: Int, pressed: Boolean): Boolean
    external fun nativeScroll(handle: Long, session: Long, window: Int, x: Float, y: Float, horizontal: Float, vertical: Float): Boolean
    external fun nativeKey(handle: Long, session: Long, window: Int, androidKeycode: Int, pressed: Boolean): Boolean
    external fun nativeCommitText(handle: Long, session: Long, window: Int, text: String): Boolean
    external fun nativeSetPreeditText(handle: Long, session: Long, window: Int, text: String, cursorBegin: Int, cursorEnd: Int): Boolean
    external fun nativeDeleteSurroundingText(handle: Long, session: Long, window: Int, beforeLength: Int, afterLength: Int): Boolean
    external fun nativeSetClipboardText(handle: Long, text: String): Boolean
    external fun nativeCloseWindow(handle: Long, session: Long, window: Int): Boolean
    external fun nativeResizeWindow(handle: Long, session: Long, window: Int, width: Int, height: Int): Boolean
    external fun nativeDisconnectSession(handle: Long, session: Long): Boolean
    external fun nativeDestroy(handle: Long)
}
