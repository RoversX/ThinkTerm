package com.roversx.thinkterm

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.SurfaceTexture
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import android.view.Surface
import android.view.TextureView
import android.view.VelocityTracker
import android.view.View
import android.view.ViewConfiguration
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.FrameLayout
import android.widget.OverScroller
import kotlin.math.abs
import kotlin.math.floor

/// The terminal itself: a TextureView the core draws on (a plain view, so
/// the overview can scale and clip it like any other), with a
/// transparent, focusable view over it for touches and the IME. Points
/// handed to the core are in dp, the unit implied by `attachSurface`'s
/// scale; the surface's own size stays in pixels.
@SuppressLint("ViewConstructor")
class TerminalHostView(context: Context, private val model: TerminalModel) : FrameLayout(context) {
    private val texture = TextureView(context)
    val input = TerminalInputView(context, model)

    private var surface: Surface? = null
    private var window: Long = 0
    private var lastSize = Triple(0, 0, 0.0)

    init {
        // Not opaque: the overview fades the terminal, and an opaque texture
        // ignores its layer's alpha when it repaints itself.
        texture.isOpaque = false
        addView(texture, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
        addView(input, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
        model.onFocusRequested = { input.focusAndShowKeyboard() }
        model.onHideKeyboard = { input.hideKeyboard() }
        texture.surfaceTextureListener = object : TextureView.SurfaceTextureListener {
            override fun onSurfaceTextureAvailable(st: SurfaceTexture, width: Int, height: Int) {
                val s = Surface(st)
                surface = s
                window = NativeWindow.fromSurface(s)
                Log.i("thinkterm", "shell: native window $window")
                attachOrResize(width, height)
            }

            override fun onSurfaceTextureSizeChanged(st: SurfaceTexture, width: Int, height: Int) {
                attachOrResize(width, height)
            }

            override fun onSurfaceTextureDestroyed(st: SurfaceTexture): Boolean {
                // The handshake: detach must return before the surface is
                // released, or the core would draw into freed memory.
                if (model.generation != 0uL) {
                    model.core.detachSurface(model.generation)
                    model.generation = 0uL
                }
                if (window != 0L) {
                    NativeWindow.release(window)
                    window = 0
                }
                surface?.release()
                surface = null
                lastSize = Triple(0, 0, 0.0)
                return true
            }

            override fun onSurfaceTextureUpdated(st: SurfaceTexture) {}
        }
    }

    /// The picture hidden or shown; touches keep arriving either way,
    /// which the overview relies on while the terminal sits in its card.
    fun showPicture(shown: Boolean) {
        texture.visibility = if (shown) View.VISIBLE else View.INVISIBLE
    }

    private fun attachOrResize(width: Int, height: Int) {
        if (window == 0L || width <= 0 || height <= 0) return
        val density = resources.displayMetrics.density.toDouble()
        if (model.generation == 0uL) {
            val gen = model.core.attachSurface(window.toULong(), width.toUInt(), height.toUInt(), density)
            model.generation = gen
            lastSize = Triple(width, height, density)
            Log.i("thinkterm", "shell: attached generation $gen (${width}x$height @$density)")
            if (gen == 0uL) Log.w("thinkterm", "shell: the core could not take the surface")
            // Keys go to the terminal from the start; the keyboard waits
            // for a tap.
            input.requestFocus()
        } else if (lastSize != Triple(width, height, density)) {
            lastSize = Triple(width, height, density)
            model.core.resize(model.generation, width.toUInt(), height.toUInt(), density)
        }
    }
}

/// Touches, hardware keys and the IME. Invisible: it only takes input.
/// A tap raises the keyboard, one finger scrolls (smooth with a fling, or
/// by whole rows), a horizontal flick switches tabs, a pinch steps the
/// text size, two fingers swiped down put the keyboard away, and a long
/// press starts a selection the finger then extends.
@SuppressLint("ViewConstructor")
class TerminalInputView(context: Context, private val model: TerminalModel) : View(context) {
    private val settings get() = model.settings
    private val slop = ViewConfiguration.get(context).scaledTouchSlop
    private val tapTimeout = ViewConfiguration.getTapTimeout().toLong() * 2
    private val longPressTimeout = ViewConfiguration.getLongPressTimeout().toLong()
    private val minFling = ViewConfiguration.get(context).scaledMinimumFlingVelocity * 4
    private val main = Handler(Looper.getMainLooper())
    private var downX = 0f
    private var downY = 0f
    private var lastY = 0f
    private var downAt = 0L
    private var dragging = false
    private var pinching = false
    private var pinchStart = 1f
    private var rowRemainder = 0f
    private var velocity: VelocityTracker? = null
    private val scroller = OverScroller(context)
    private var flingLast = 0
    /// Two fingers: their mean y when they landed, for the swipe down.
    private var twoFingerStartY: Float? = null
    private var twoFingerDone = false

    // MARK: selection

    /// The end the finger moves while selecting; null while not selecting.
    private var selecting = false
    private var anchor: Pair<Int, Int>? = null
    private var head: Pair<Int, Int>? = null
    private var screen: ScreenText? = null
    private val longPress = Runnable { startSelection() }

    /// The composition the IME is building. Nothing reaches the pane
    /// while it is open; it is sent once, when it is committed.
    private var composing: String? = null

    /// While the overview is up the shrunken terminal is a card: any
    /// touch on it is a tap on the card, not input.
    var touchOverride: (() -> Unit)? = null

    private val density: Float get() = resources.displayMetrics.density

    private val pinch = ScaleGestureDetector(context, object : ScaleGestureDetector.SimpleOnScaleGestureListener() {
        override fun onScaleBegin(d: ScaleGestureDetector): Boolean {
            if (!settings.pinchZoom) return false
            pinchStart = 1f
            pinching = true
            dropLongPress()
            return true
        }

        override fun onScale(d: ScaleGestureDetector): Boolean {
            // One font step per 15% of scale, either way, as on iOS.
            while (d.scaleFactor > pinchStart * 1.15f) {
                pinchStart *= 1.15f
                model.stepFont(1.0)
            }
            while (d.scaleFactor < pinchStart / 1.15f) {
                pinchStart /= 1.15f
                model.stepFont(-1.0)
            }
            return true
        }

        override fun onScaleEnd(d: ScaleGestureDetector) {
            pinching = false
        }
    })

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        isClickable = true
        model.onScreenChanged = { if (anchor != null) screen = null }
    }

    fun focusAndShowKeyboard() {
        requestFocus()
        val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        imm.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
    }

    fun hideKeyboard() {
        val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        imm.hideSoftInputFromWindow(windowToken, 0)
    }

    // MARK: touches

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(event: MotionEvent): Boolean {
        touchOverride?.let {
            if (event.actionMasked == MotionEvent.ACTION_UP) it()
            return true
        }
        pinch.onTouchEvent(event)
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                scroller.forceFinished(true)
                velocity?.recycle()
                velocity = VelocityTracker.obtain().also { it.addMovement(event) }
                downX = event.x
                downY = event.y
                lastY = event.y
                downAt = event.eventTime
                dragging = false
                selecting = false
                twoFingerStartY = null
                twoFingerDone = false
                // A press on an end of a standing selection moves that end.
                if (anchor != null && grabEnd(event.x, event.y)) {
                    selecting = true
                } else if (settings.longPressSelects) {
                    main.postDelayed(longPress, longPressTimeout)
                }
            }

            MotionEvent.ACTION_POINTER_DOWN -> {
                dropLongPress()
                if (event.pointerCount == 2 && !selecting) {
                    twoFingerStartY = (event.getY(0) + event.getY(1)) / 2
                }
            }

            MotionEvent.ACTION_MOVE -> {
                velocity?.addMovement(event)
                if (selecting) {
                    extendSelection(event.x, event.y)
                    return true
                }
                if (event.pointerCount > 1) {
                    twoFinger(event)
                    return true
                }
                if (pinching) return true
                if (!dragging && (abs(event.y - downY) > slop || abs(event.x - downX) > slop)) {
                    dragging = true
                    dropLongPress()
                }
                if (dragging) {
                    // A busy main thread gets the moves coalesced: the
                    // samples in between ride along as history.
                    for (h in 0 until event.historySize) scrollTo(event.getHistoricalY(h))
                    scrollTo(event.y)
                }
            }

            MotionEvent.ACTION_UP -> {
                dropLongPress()
                if (selecting) {
                    selecting = false
                    finishSelection()
                    return true
                }
                if (dragging) {
                    scrollTo(event.y)
                    val dx = event.x - downX
                    val dy = event.y - downY
                    if (abs(dx) > 80 * density && abs(dx) > 2 * abs(dy)) {
                        // A sideways flick: the next tab along.
                        model.switchTab(if (dx < 0) 1 else -1)
                    } else if (settings.smoothScroll) {
                        fling()
                    }
                } else if (!pinching && twoFingerStartY == null && event.eventTime - downAt < tapTimeout) {
                    tap()
                }
                velocity?.recycle()
                velocity = null
            }

            MotionEvent.ACTION_CANCEL -> {
                dropLongPress()
                selecting = false
                velocity?.recycle()
                velocity = null
            }
        }
        return true
    }

    private fun tap() {
        if (anchor != null) {
            // A tap outside a selection puts it away; the keyboard stays.
            clearSelection()
            return
        }
        val x = (downX / density).toDouble()
        val y = (downY / density).toDouble()
        model.pointer("down", x, y)
        model.pointer("up", x, y)
        focusAndShowKeyboard()
    }

    /// The finger is now at `y`: the rows between there and where it
    /// was go past. Dragging up moves towards the newest row, which is
    /// what the core calls positive travel.
    private fun scrollTo(y: Float) {
        val px = (lastY - y) / density
        lastY = y
        if (px == 0f) return
        val cx = (width / 2f / density).toDouble()
        val cy = (height / 2f / density).toDouble()
        if (settings.smoothScroll) {
            model.wheelPx(cx, cy, px.toDouble())
        } else {
            // Whole rows, at the App's own row height.
            val row = model.cellHeight.toFloat().coerceAtLeast(1f)
            rowRemainder += px
            val rows = (rowRemainder / row).toInt()
            if (rows != 0) {
                rowRemainder -= rows * row
                model.wheel(cx, cy, -rows.toDouble())
            }
        }
    }

    /// The finger lifted at speed: the scroll carries on and slows.
    private fun fling() {
        val v = velocity ?: return
        v.computeCurrentVelocity(1000)
        val vy = v.yVelocity
        if (abs(vy) < minFling) return
        flingLast = 0
        scroller.fling(0, 0, 0, (-vy).toInt(), 0, 0, Int.MIN_VALUE / 2, Int.MAX_VALUE / 2)
        postOnAnimation(flingStep)
    }

    private val flingStep = object : Runnable {
        override fun run() {
            if (!scroller.computeScrollOffset()) return
            val y = scroller.currY
            val dy = y - flingLast
            flingLast = y
            if (dy != 0) {
                model.wheelPx((width / 2f / density).toDouble(), (height / 2f / density).toDouble(), (dy / density).toDouble())
            }
            postOnAnimation(this)
        }
    }

    /// Two fingers moving together, down: the keyboard goes away.
    private fun twoFinger(event: MotionEvent) {
        val start = twoFingerStartY ?: return
        if (twoFingerDone || !settings.twoFingerHidesKeyboard || event.pointerCount < 2) return
        val y = (event.getY(0) + event.getY(1)) / 2
        if (y - start > 48 * density) {
            twoFingerDone = true
            hideKeyboard()
        }
    }

    // MARK: selection

    private fun dropLongPress() = main.removeCallbacks(longPress)

    private fun cellAt(x: Float, y: Float): Pair<Int, Int>? {
        val s = screen ?: Views.screenText(model.core.screenText())?.also { screen = it } ?: return null
        if (s.rows <= 0 || s.cols <= 0) return null
        val col = floor((x / density - s.originX) / s.cellW).toInt().coerceIn(0, s.cols - 1)
        val row = floor((y / density - s.originY) / s.cellH).toInt().coerceIn(0, s.rows - 1)
        return Pair(row, col)
    }

    private fun startSelection() {
        if (dragging || pinching || twoFingerStartY != null) return
        val cell = cellAt(downX, downY) ?: return
        selecting = true
        anchor = cell
        head = cell
        performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
        model.core.setSelection(cell.first.toUInt(), cell.second.toUInt(), cell.first.toUInt(), cell.second.toUInt())
        model.selectionMenuAt = null
    }

    private fun extendSelection(x: Float, y: Float) {
        val a = anchor ?: return
        val cell = cellAt(x, y) ?: return
        if (cell == head) return
        head = cell
        model.core.setSelection(a.first.toUInt(), a.second.toUInt(), cell.first.toUInt(), cell.second.toUInt())
    }

    /// A press within a cell of an end takes that end along; the other
    /// end becomes the anchor.
    private fun grabEnd(x: Float, y: Float): Boolean {
        val cell = cellAt(x, y) ?: return false
        val a = anchor ?: return false
        val h = head ?: return false
        fun near(p: Pair<Int, Int>) = abs(p.first - cell.first) <= 1 && abs(p.second - cell.second) <= 1
        return when {
            near(h) -> true
            near(a) -> { anchor = h; head = a; true }
            else -> false
        }
    }

    private fun finishSelection() {
        val s = screen ?: return
        val h = head ?: return
        model.selectionMenuAt = Pair(s.originX + h.second * s.cellW, s.originY + h.first * s.cellH)
    }

    fun clearSelection() {
        anchor = null
        head = null
        screen = null
        model.clearSelection()
    }

    // MARK: hardware keys (and `adb shell input text`)

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        val ctrl = event.isCtrlPressed
        val alt = event.isAltPressed
        val shift = event.isShiftPressed
        val name = domName(keyCode)
        if (name != null) {
            model.key(name, ctrl, alt, shift)
            return true
        }
        val ch = event.unicodeChar
        if (ch != 0) {
            val s = ch.toChar().toString()
            if (ctrl || alt) model.key(s, ctrl, alt, false) else model.text(s)
            return true
        }
        return super.onKeyDown(keyCode, event)
    }

    private fun domName(keyCode: Int): String? = when (keyCode) {
        KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> "Enter"
        KeyEvent.KEYCODE_DEL -> "Backspace"
        KeyEvent.KEYCODE_FORWARD_DEL -> "Delete"
        KeyEvent.KEYCODE_TAB -> "Tab"
        KeyEvent.KEYCODE_ESCAPE -> "Escape"
        KeyEvent.KEYCODE_DPAD_UP -> "ArrowUp"
        KeyEvent.KEYCODE_DPAD_DOWN -> "ArrowDown"
        KeyEvent.KEYCODE_DPAD_LEFT -> "ArrowLeft"
        KeyEvent.KEYCODE_DPAD_RIGHT -> "ArrowRight"
        KeyEvent.KEYCODE_MOVE_HOME -> "Home"
        KeyEvent.KEYCODE_MOVE_END -> "End"
        KeyEvent.KEYCODE_PAGE_UP -> "PageUp"
        KeyEvent.KEYCODE_PAGE_DOWN -> "PageDown"
        KeyEvent.KEYCODE_INSERT -> "Insert"
        KeyEvent.KEYCODE_F1 -> "F1"
        KeyEvent.KEYCODE_F2 -> "F2"
        KeyEvent.KEYCODE_F3 -> "F3"
        KeyEvent.KEYCODE_F4 -> "F4"
        KeyEvent.KEYCODE_F5 -> "F5"
        KeyEvent.KEYCODE_F6 -> "F6"
        KeyEvent.KEYCODE_F7 -> "F7"
        KeyEvent.KEYCODE_F8 -> "F8"
        KeyEvent.KEYCODE_F9 -> "F9"
        KeyEvent.KEYCODE_F10 -> "F10"
        KeyEvent.KEYCODE_F11 -> "F11"
        KeyEvent.KEYCODE_F12 -> "F12"
        else -> null
    }

    // MARK: the IME

    override fun onCheckIsTextEditor(): Boolean = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        // No suggestions and no autocorrect: a terminal wants the keys as
        // typed, and the password variation is the one every IME honours.
        outAttrs.inputType = EditorInfo.TYPE_CLASS_TEXT or
            EditorInfo.TYPE_TEXT_FLAG_NO_SUGGESTIONS or
            EditorInfo.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
        outAttrs.imeOptions = EditorInfo.IME_ACTION_NONE or EditorInfo.IME_FLAG_NO_EXTRACT_UI
        return Connection(this)
    }

    private inner class Connection(view: View) : BaseInputConnection(view, false) {
        override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
            val s = text?.toString() ?: return true
            endComposing()
            if (s == "\n") model.key("Enter") else if (s.isNotEmpty()) model.text(s)
            return true
        }

        override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
            val s = text?.toString() ?: ""
            if (s.isEmpty()) {
                // The IME cleared the composition (backspaced it away).
                if (composing != null) {
                    composing = null
                    model.composing = null
                    model.core.setComposing(false)
                }
                return true
            }
            if (composing == null) model.core.setComposing(true)
            composing = s
            model.composing = s
            return true
        }

        override fun finishComposingText(): Boolean {
            val s = composing
            composing = null
            model.composing = null
            if (s != null) {
                model.core.setComposing(false)
                if (s.isNotEmpty()) model.text(s)
            }
            return true
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            // While composing the IME edits its own buffer, not the pane.
            if (composing != null) return true
            repeat(beforeLength) { model.key("Backspace") }
            return true
        }

        override fun sendKeyEvent(event: KeyEvent): Boolean {
            if (event.action != KeyEvent.ACTION_DOWN) return true
            return onKeyDown(event.keyCode, event)
        }

        override fun performEditorAction(actionCode: Int): Boolean {
            model.key("Enter")
            return true
        }

        private fun endComposing() {
            if (composing == null) return
            composing = null
            model.composing = null
            model.core.setComposing(false)
        }
    }
}
