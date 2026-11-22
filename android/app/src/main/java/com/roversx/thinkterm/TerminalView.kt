package com.roversx.thinkterm

import android.annotation.SuppressLint
import android.content.Context
import android.content.res.Configuration
import android.graphics.SurfaceTexture
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.HapticFeedbackConstants
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.PointerIcon
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
        // The App asks for focus after a press, a finished rename and
        // every answered request; on a phone that must not raise the
        // keyboard (a tap does that itself), only keep it where it is.
        model.onFocusRequested = { if (input.keyboardShown()) input.requestFocus() }
        model.onHideKeyboard = { input.hideKeyboard() }
        model.onShowKeyboard = { input.focusAndShowKeyboard() }
        texture.surfaceTextureListener = object : TextureView.SurfaceTextureListener {
            override fun onSurfaceTextureAvailable(st: SurfaceTexture, width: Int, height: Int) {
                // The surface lent while the host was only listed goes first.
                model.dropListSurface()
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
    private val longPressTimeout = ViewConfiguration.getLongPressTimeout().toLong()
    private val minFling = ViewConfiguration.get(context).scaledMinimumFlingVelocity * 4
    private val main = Handler(Looper.getMainLooper())
    private var downX = 0f
    private var downY = 0f
    private var lastY = 0f
    private var dragging = false
    private var pinching = false
    private var pinchStart = 1f
    /// The fingers' distance when the pinch began: the scale is measured
    /// from there, not from one event to the next.
    private var pinchSpan = 0f
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

    /// What of a word the keyboard composes in place (`composesInPlace`)
    /// has gone to the terminal already; null while it composes none.
    private var sent: String? = null

    /// The keyboard itself is typing, through `Connection`: what it sends
    /// does not disturb the word it is making (`lineDisturbed`).
    private var imeSending = false

    /// While the overview is up the shrunken terminal is a card: any
    /// touch on it is a tap on the card, not input.
    var touchOverride: (() -> Unit)? = null

    private val density: Float get() = resources.displayMetrics.density

    private val pinch = ScaleGestureDetector(context, object : ScaleGestureDetector.SimpleOnScaleGestureListener() {
        override fun onScaleBegin(d: ScaleGestureDetector): Boolean {
            if (!settings.pinchZoom) return false
            pinchStart = 1f
            pinchSpan = d.currentSpan
            pinching = true
            dropLongPress()
            return true
        }

        override fun onScale(d: ScaleGestureDetector): Boolean {
            // One font step per 15% of scale, either way, as on iOS.
            if (pinchSpan <= 0f) return true
            val scale = d.currentSpan / pinchSpan
            while (scale > pinchStart * 1.15f) {
                pinchStart *= 1.15f
                model.stepFont(1.0)
            }
            while (scale < pinchStart / 1.15f) {
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
        // A mouse over the terminal shows the text cursor, as the desktop's.
        pointerIcon = PointerIcon.getSystemIcon(context, PointerIcon.TYPE_TEXT)
        model.onScreenChanged = { if (anchor != null) screen = null }
        model.onDropComposition = { dropComposition() }
        model.onImeOptionsChanged = {
            val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
            imm.restartInput(this)
        }
        model.onLineDisturbed = { lineDisturbed() }
        model.onSelectionGone = {
            anchor = null
            head = null
            screen = null
        }
    }

    fun keyboardShown(): Boolean =
        rootWindowInsets?.isVisible(android.view.WindowInsets.Type.ime()) == true

    /// The connection changed: an open composition is let go here and in
    /// the core, whose latch would otherwise swallow every key of the
    /// next connection, and the IME starts over.
    private fun dropComposition() {
        val open = composing != null || sent != null
        composing = null
        sent = null
        model.composing = null
        model.core.setComposing(false)
        if (open) {
            val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
            imm.restartInput(this)
        }
    }

    /// Something other than the keyboard typed, or another pane has the
    /// line: a word the keyboard was making in place stays on the terminal
    /// as it is, and the keyboard starts afresh, so a correction or a
    /// backspace of that word does not reach into what came after it.
    private fun lineDisturbed() {
        if (!imeSending) letGoOfWord()
    }

    /// A word the keyboard was making in place stays on the terminal as it
    /// is, and the keyboard starts afresh.
    private fun letGoOfWord() {
        if (sent == null) return
        sent = null
        val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        imm.restartInput(this)
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
        if (isMouse(event)) return mouseTouch(event)
        pinch.onTouchEvent(event)
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                scroller.forceFinished(true)
                velocity?.recycle()
                velocity = VelocityTracker.obtain().also { it.addMovement(event) }
                downX = event.x
                downY = event.y
                lastY = event.y
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

            MotionEvent.ACTION_POINTER_UP -> {
                // The finger that stays is the one the scroll follows
                // from here, or the rows between the two would go past
                // in one jump. Its lift is no tap: the two-finger mark
                // stays set.
                if (event.pointerCount == 2) {
                    val staying = if (event.actionIndex == 0) 1 else 0
                    downX = event.getX(staying)
                    downY = event.getY(staying)
                    lastY = downY
                    dragging = false
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
                } else if (!pinching && twoFingerStartY == null) {
                    // Any press that neither dragged nor turned into a
                    // long press is a tap, however long it was held.
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
        // A chip a right click opened, with nothing selected, goes too.
        model.selectionMenuAt = null
        val x = (downX / density).toDouble()
        val y = (downY / density).toDouble()
        // The tap may give another pane the focus.
        lineDisturbed()
        model.pointer("down", x, y)
        model.pointer("up", x, y)
        // With a keyboard of its own on the device the screen's has no
        // place: the keys come from there.
        if (hardKeyboard()) requestFocus() else focusAndShowKeyboard()
    }

    // MARK: a mouse (desktop mode, a tablet's trackpad)

    /// The button held down, as the core counts them (0 left, 1 middle,
    /// 2 right), or none.
    private var mouseButton: Int? = null
    /// A right press is down that opens the copy and paste chip on its
    /// release, rather than going to the program under it.
    private var clipChipPress = false

    private fun isMouse(event: MotionEvent): Boolean =
        event.getToolType(0) == MotionEvent.TOOL_TYPE_MOUSE || event.isFromSource(InputDevice.SOURCE_MOUSE)

    /// `heldBack`: no program's, should the one under it ask for the mouse,
    /// as Shift holds an event back from it.
    private fun mouse(kind: String, event: MotionEvent, button: Int, held: Boolean, heldBack: Boolean = false) {
        model.core.mouse(
            kind,
            (event.x / density).toDouble(),
            (event.y / density).toDouble(),
            button.toUByte(),
            held,
            heldBack || event.metaState and KeyEvent.META_SHIFT_ON != 0,
            event.metaState and KeyEvent.META_CTRL_ON != 0,
            event.metaState and KeyEvent.META_ALT_ON != 0,
        )
    }

    /// A mouse is the desktop's: the core takes its presses, drags and
    /// clicks as the desktop does -- the focus, a divider, a selection by
    /// drag or by a double or triple click, the mouse a program asked for.
    /// The keyboard on screen stays down; a selection made offers its
    /// copy chip where the button let go.
    private fun mouseTouch(event: MotionEvent): Boolean {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                requestFocus()
                // A right press keeps the selection for the chip it opens.
                if (event.buttonState and MotionEvent.BUTTON_PRIMARY != 0 ||
                    event.buttonState and MotionEvent.BUTTON_SECONDARY == 0
                ) {
                    if (anchor != null) clearSelection()
                    model.selectionMenuAt = null
                }
            }
            MotionEvent.ACTION_MOVE -> mouseButton?.let { mouse("move", event, it, true) }
            // The release, should no button release come before it. A
            // cancel ends the chip's press without its chip.
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                if (clipChipPress) clipChipRelease(event, opens = event.actionMasked == MotionEvent.ACTION_UP)
                mouseButton?.let { release(event, it) }
            }
        }
        return true
    }

    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        if (!isMouse(event)) return super.onGenericMotionEvent(event)
        when (event.actionMasked) {
            MotionEvent.ACTION_BUTTON_PRESS -> {
                val button = coreButton(event.actionButton) ?: return super.onGenericMotionEvent(event)
                // A press the chip took that nothing ended is over: the
                // release that comes next is this press's.
                if (clipChipPress) clipChipRelease(event, opens = false)
                // A press may put another pane in focus: a word the
                // keyboard was making stays where it was typed.
                lineDisturbed()
                if (button == 2 && opensClipChip(event)) {
                    // The terminal's own copy and paste, where a right click
                    // opens the desktop's menu; it comes up on the release.
                    // The core still takes the press, held back from any
                    // program: it focuses the pane under it, the one Copy
                    // and Paste then act on, and does nothing more with it.
                    clipChipPress = true
                    mouse("down", event, button, true, heldBack = true)
                    return true
                }
                if (button == 2) {
                    // The program under it takes the press: a selection of
                    // the terminal's own goes, and its chip, as for any.
                    if (anchor != null) clearSelection()
                    model.selectionMenuAt = null
                }
                mouseButton = button
                mouse("down", event, button, true)
            }
            MotionEvent.ACTION_BUTTON_RELEASE -> {
                val button = coreButton(event.actionButton) ?: return super.onGenericMotionEvent(event)
                if (button == 2 && clipChipPress) {
                    clipChipRelease(event, opens = true)
                    return true
                }
                release(event, button)
            }
            // A hover: no button down. The core shows what is under it to a
            // program that follows the mouse.
            MotionEvent.ACTION_HOVER_MOVE -> mouse("move", event, mouseButton ?: 0, false)
            MotionEvent.ACTION_SCROLL -> {
                // A notch up is back into the history, three rows of it, as
                // the desktop scrolls.
                val notches = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
                if (notches != 0f) {
                    model.wheel((event.x / density).toDouble(), (event.y / density).toDouble(), notches * 3.0)
                }
            }
            else -> return super.onGenericMotionEvent(event)
        }
        return true
    }

    private fun release(event: MotionEvent, button: Int) {
        mouse("up", event, button, false)
        mouseButton = null
        if (button == 0 && !model.core.selectedText().isNullOrEmpty()) {
            model.openClipChip((event.x / density).toDouble(), (event.y / density).toDouble(), copies = true)
        }
    }

    /// The end of a right press the chip took: the core has its release as
    /// it had the press, the core's and no program's, and the chip comes up
    /// where it let go -- unless `opens` is false, for a press cancelled.
    private fun clipChipRelease(event: MotionEvent, opens: Boolean) {
        clipChipPress = false
        mouse("up", event, 2, false, heldBack = true)
        if (!opens) return
        model.openClipChip(
            (event.x / density).toDouble(),
            (event.y / density).toDouble(),
            copies = !model.core.selectedText().isNullOrEmpty(),
        )
    }

    /// A right press the chip takes: the program under it did not ask for
    /// the mouse, or Shift holds the press back from it, as on the desktop.
    private fun opensClipChip(event: MotionEvent): Boolean =
        event.metaState and KeyEvent.META_SHIFT_ON != 0 ||
            !model.core.mouseGrabbedAt((event.x / density).toDouble(), (event.y / density).toDouble())

    private fun coreButton(button: Int): Int? = when (button) {
        MotionEvent.BUTTON_PRIMARY -> 0
        MotionEvent.BUTTON_TERTIARY -> 1
        MotionEvent.BUTTON_SECONDARY -> 2
        else -> null
    }

    /// The device has a keyboard of its own, open.
    private fun hardKeyboard(): Boolean = resources.configuration.let {
        it.keyboard == Configuration.KEYBOARD_QWERTY && it.hardKeyboardHidden == Configuration.HARDKEYBOARDHIDDEN_NO
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
        model.openClipChip(s.originX + h.second * s.cellW, s.originY + h.first * s.cellH, copies = true)
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
        // A keyboard's own Paste and Copy keys.
        when (keyCode) {
            KeyEvent.KEYCODE_PASTE -> {
                pasteKey()
                return true
            }
            KeyEvent.KEYCODE_COPY -> {
                if (!model.core.selectedText().isNullOrEmpty()) model.copySelection()
                return true
            }
        }
        // A terminal's own copy and paste, Ctrl+C being the shell's.
        if (ctrl && shift && !alt) {
            when (keyCode) {
                KeyEvent.KEYCODE_C -> {
                    if (!model.core.selectedText().isNullOrEmpty()) {
                        model.copySelection()
                        model.showCopied()
                    }
                    return true
                }
                KeyEvent.KEYCODE_V -> { pasteKey(); return true }
            }
        }
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

    /// A Paste key, the on-screen keyboard's too (`sendKeyEvent`): the text
    /// is not part of the word that keyboard was making in place, which it
    /// lets go of first, as for any paste.
    private fun pasteKey() {
        letGoOfWord()
        model.pasteFromClipboard()
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
        // typed. Not the password variation, though: keyboards keep to a
        // Latin layout there, hiding the Chinese and Japanese ones the
        // composition below is for.
        outAttrs.inputType = EditorInfo.TYPE_CLASS_TEXT or
            EditorInfo.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_ACTION_NONE or EditorInfo.IME_FLAG_NO_EXTRACT_UI
        if (settings.incognitoKeyboard) {
            // What is typed into a shell is not the keyboard's to learn;
            // Gboard shows its incognito look for this flag.
            outAttrs.imeOptions = outAttrs.imeOptions or EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
        }
        // A keyboard starting on the view starts with no word of its own.
        sent = null
        return Connection(this)
    }

    /// What the keyboard sends, marked as its own while it goes.
    private inline fun fromIme(send: () -> Unit) {
        imeSending = true
        try {
            send()
        } finally {
            imeSending = false
        }
    }

    private inner class Connection(view: View) : BaseInputConnection(view, false) {
        override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
            val s = text?.toString() ?: return true
            val inPlace = sent
            if (inPlace != null) {
                // The commit takes the composition's place, as on any field.
                sent = null
                fromIme {
                    if (s == "\n") {
                        sendChange(inPlace, "")
                        model.key("Enter")
                    } else if (isPasted(s) && !hasLineBreak(inPlace)) {
                        // A composition with a line in it ran that line on
                        // the way; taken back and pasted, it would run twice.
                        sendChange(inPlace, "")
                        model.paste(s)
                    } else {
                        sendChange(inPlace, s)
                    }
                }
                return true
            }
            endComposing()
            fromIme {
                when {
                    s == "\n" -> model.key("Enter")
                    isPasted(s) -> model.paste(s)
                    s.isNotEmpty() -> model.text(s)
                }
            }
            return true
        }

        /// Text a keyboard commits with more than one line in it is a
        /// paste: from its clipboard strip, say. Typed, each line would run
        /// as it arrived. One line ending in a line break is typed, as it
        /// was: a keyboard may send a word together with its Enter.
        private fun isPasted(s: String): Boolean = hasLineBreak(s.removeSuffix("\n").removeSuffix("\r"))

        private fun hasLineBreak(s: String): Boolean = s.contains('\n') || s.contains('\r')

        /// The keyboard's own Paste and Copy, as on its editing panel.
        override fun performContextMenuAction(id: Int): Boolean {
            when (id) {
                android.R.id.paste, android.R.id.pasteAsPlainText -> {
                    // As the key bar's Paste: a word the keyboard was making
                    // in place is let go of first (`lineDisturbed`).
                    endComposing()
                    model.pasteFromClipboard()
                }
                android.R.id.copy -> {
                    if (model.core.selectedText().isNullOrEmpty()) return false
                    model.copySelection()
                }
                else -> return false
            }
            return true
        }

        override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
            val s = text?.toString() ?: ""
            val inPlace = sent
            if (inPlace != null || (composing == null && s.isNotEmpty() && composesInPlace())) {
                fromIme { sendChange(inPlace ?: "", s) }
                sent = s.ifEmpty { null }
                return true
            }
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
            // Composed in place: it is on the terminal already.
            sent = null
            val s = composing
            composing = null
            model.composing = null
            if (s != null) {
                model.core.setComposing(false)
                if (s.isNotEmpty()) fromIme { model.text(s) }
            }
            return true
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            // While composing the IME edits its own buffer, not the pane.
            if (composing != null || sent != null) return true
            fromIme { repeat(beforeLength) { model.key("Backspace") } }
            return true
        }

        override fun sendKeyEvent(event: KeyEvent): Boolean {
            if (event.action != KeyEvent.ACTION_DOWN) return true
            var handled = false
            fromIme { handled = onKeyDown(event.keyCode, event) }
            return handled
        }

        override fun performEditorAction(actionCode: Int): Boolean {
            fromIme { model.key("Enter") }
            return true
        }

        private fun endComposing() {
            if (composing == null) return
            composing = null
            model.composing = null
            model.core.setComposing(false)
        }

        /// Whether the keyboard on screen composes words in its own script:
        /// Latin, Cyrillic, Greek... A keyboard that does (Samsung's,
        /// SwiftKey's, even with suggestions off) would hold `j` or `q` back
        /// from vim or less until a space; what it composes goes to the
        /// terminal as it is typed instead. Chinese, Japanese and Korean
        /// compose into other characters, and so does a keyboard that names
        /// no language: theirs waits to be committed.
        private fun composesInPlace(): Boolean {
            val imm = context.getSystemService(InputMethodManager::class.java) ?: return false
            val subtype = imm.currentInputMethodSubtype ?: return false
            @Suppress("DEPRECATION")
            val tag = subtype.languageTag.ifEmpty { subtype.locale }
            if (tag.isEmpty()) return false
            val language = tag.substringBefore('-').substringBefore('_').lowercase()
            return language !in setOf("zh", "ja", "ko")
        }

        /// The terminal holds `from` of a composition and is to hold `to`:
        /// what differs at the end goes as backspaces and the new end.
        private fun sendChange(from: String, to: String) {
            var same = 0
            val most = minOf(from.length, to.length)
            while (same < most && from[same] == to[same]) same++
            // Never half a surrogate pair kept.
            if (same > 0 && Character.isHighSurrogate(from[same - 1])) same--
            repeat(from.codePointCount(same, from.length)) { model.key("Backspace") }
            if (same < to.length) model.text(to.substring(same))
        }
    }
}
