package com.roversx.thinkterm

import android.annotation.SuppressLint
import android.content.Context
import android.util.Log
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewConfiguration
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.FrameLayout

/// The terminal itself: a SurfaceView the core draws on, with a
/// transparent, focusable view over it for touches and the IME. Points
/// handed to the core are in dp, the unit implied by `attachSurface`'s
/// scale; the surface's own size stays in pixels.
@SuppressLint("ViewConstructor")
class TerminalHostView(context: Context, private val model: TerminalModel) : FrameLayout(context) {
    private val surfaceView = SurfaceView(context)
    private val input = TerminalInputView(context, model)

    private var window: Long = 0
    private var lastSize = Triple(0, 0, 0.0)

    init {
        addView(surfaceView, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
        addView(input, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
        model.onFocusRequested = { input.focusAndShowKeyboard() }
        surfaceView.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {
                window = NativeWindow.fromSurface(holder.surface)
                Log.i("thinkterm", "shell: native window $window")
            }

            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
                if (window == 0L) return
                val density = resources.displayMetrics.density.toDouble()
                if (model.generation == 0uL) {
                    val gen = model.core.attachSurface(window.toULong(), width.toUInt(), height.toUInt(), density)
                    model.generation = gen
                    lastSize = Triple(width, height, density)
                    Log.i("thinkterm", "shell: attached generation $gen (${width}x$height @$density)")
                    if (gen == 0uL) {
                        Log.w("thinkterm", "shell: the core could not take the surface")
                    } else {
                        input.focusAndShowKeyboard()
                    }
                } else if (lastSize != Triple(width, height, density)) {
                    lastSize = Triple(width, height, density)
                    model.core.resize(model.generation, width.toUInt(), height.toUInt(), density)
                }
            }

            override fun surfaceDestroyed(holder: SurfaceHolder) {
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
                lastSize = Triple(0, 0, 0.0)
            }
        })
    }
}

/// Touches, hardware keys and the IME. Invisible: it only takes input.
@SuppressLint("ViewConstructor")
class TerminalInputView(context: Context, private val model: TerminalModel) : View(context) {
    private val slop = ViewConfiguration.get(context).scaledTouchSlop
    private val tapTimeout = ViewConfiguration.getTapTimeout().toLong() * 2
    private var downX = 0f
    private var downY = 0f
    private var lastY = 0f
    private var downAt = 0L
    private var dragging = false
    private var pinchStart = 1f

    /// The composition the IME is building. Nothing reaches the pane
    /// while it is open; it is sent once, when it is committed.
    private var composing: String? = null

    private val density: Float get() = resources.displayMetrics.density

    private val pinch = ScaleGestureDetector(context, object : ScaleGestureDetector.SimpleOnScaleGestureListener() {
        override fun onScaleBegin(d: ScaleGestureDetector): Boolean {
            pinchStart = 1f
            return true
        }

        override fun onScale(d: ScaleGestureDetector): Boolean {
            // One font step per 15% of scale, either way, as on iOS.
            while (d.scaleFactor > pinchStart * 1.15f) {
                pinchStart *= 1.15f
                model.core.stepFont(1.0)
            }
            while (d.scaleFactor < pinchStart / 1.15f) {
                pinchStart /= 1.15f
                model.core.stepFont(-1.0)
            }
            return true
        }
    })

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        isClickable = true
    }

    fun focusAndShowKeyboard() {
        requestFocus()
        val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        imm.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
    }

    // MARK: touches

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(event: MotionEvent): Boolean {
        pinch.onTouchEvent(event)
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                downX = event.x
                downY = event.y
                lastY = event.y
                downAt = event.eventTime
                dragging = false
            }

            MotionEvent.ACTION_MOVE -> {
                if (event.pointerCount > 1) return true
                if (!dragging && Math.abs(event.y - downY) > slop) dragging = true
                if (dragging) {
                    // A busy main thread gets the moves coalesced: the
                    // samples in between ride along as history.
                    for (h in 0 until event.historySize) scrollTo(event.getHistoricalY(h))
                    scrollTo(event.y)
                }
            }

            MotionEvent.ACTION_UP -> {
                // The lift carries the last of the travel.
                if (dragging) scrollTo(event.y)
                if (!dragging && event.eventTime - downAt < tapTimeout) {
                    val x = (downX / density).toDouble()
                    val y = (downY / density).toDouble()
                    model.core.pointer("down", x, y)
                    model.core.pointer("up", x, y)
                    focusAndShowKeyboard()
                }
            }
        }
        return true
    }

    /// The finger is now at `y`: the rows between there and where it
    /// was go past. Dragging up moves towards the newest row, which is
    /// what the core calls positive travel.
    private fun scrollTo(y: Float) {
        val px = (lastY - y) / density
        lastY = y
        if (px == 0f) return
        model.core.wheelPx(
            (width / 2f / density).toDouble(),
            (height / 2f / density).toDouble(),
            px.toDouble(),
        )
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
                    model.core.setComposing(false)
                }
                return true
            }
            if (composing == null) model.core.setComposing(true)
            composing = s
            return true
        }

        override fun finishComposingText(): Boolean {
            val s = composing
            composing = null
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
            model.core.setComposing(false)
        }
    }
}
