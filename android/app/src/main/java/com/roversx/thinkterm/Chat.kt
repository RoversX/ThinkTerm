package com.roversx.thinkterm

import android.graphics.BitmapFactory
import android.util.Base64
import android.view.HapticFeedbackConstants
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.Image
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.Crossfade
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.animation.expandVertically
import androidx.compose.animation.shrinkVertically
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.minimumInteractiveComponentSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.snapshotFlow
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withLink
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import com.kyant.backdrop.Backdrop
import com.kyant.backdrop.backdrops.layerBackdrop
import com.kyant.backdrop.backdrops.rememberLayerBackdrop
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.gestures.detectTransformGestures
import androidx.compose.ui.graphics.asAndroidBitmap
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.gestures.detectHorizontalDragGestures
import androidx.compose.animation.core.animate
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.spring
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.graphics.isSpecified
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.foundation.layout.isImeVisible
import androidx.compose.foundation.layout.navigationBars

// MARK: the chat as the core makes it (thinkterm_agent_chat::view::ChatView)

data class ChatView(
    /// The pane the chat is with: a view of a chat shown before is told
    /// from one of the chat on show by it.
    val pane: Int,
    val agent: String,
    val title: String,
    /// "connecting", "working", "idle", "blocked" or "problem".
    val state: String,
    val stateLabel: String,
    val since: Long?,
    val busy: String?,
    val guessed: String?,
    val rows: List<ChatRow>,
    val more: Boolean,
    val moreLabel: String,
    val loadingOlder: Boolean,
    val ask: ChatAsk?,
    val problem: ChatProblem?,
    val notice: ChatNotice?,
    val placeholder: String,
    val canSend: Boolean,
    val canStop: Boolean,
    /// Files and pictures can be sent along.
    val canAttach: Boolean,
    /// The model in use and the others; null when the agent does not say.
    val model: ChatModelMenu?,
    /// What a `/` (or `$`) typed in the field offers.
    val commands: List<ChatCommand>,
    /// How full the model's context is: the ring in the bar and its sheet;
    /// null when the agent does not say.
    val context: ChatContext?,
    /// The agent runs an older ThinkTerm extension than the one in its
    /// folder: said over the field, with the button that loads the new one.
    val outdated: ChatOutdated?,
    /// When each row came about (unix seconds), by row id, where known.
    val times: Map<String, Long> = emptyMap(),
    /// The replies still being written, by row id.
    val writing: Set<String> = emptySet(),
)

data class ChatOutdated(val text: String, val action: ChatAction)

/// How full the context is (`fill` 0 to 1, null while not known; `level`
/// "ok", "high" or "full"), what fills it, and the session's totals.
data class ChatContext(
    val fill: Float?,
    val level: String,
    val percent: String,
    val heading: String,
    val amount: String,
    val parts: List<ChatContextPart>,
    val breakdown: String,
    val compactsAt: String?,
    val note: String?,
    val totalsHeading: String,
    val totals: List<Pair<String, String>>,
    val compact: String?,
)

/// One part of the context: what it is ("system", "user", ... "free"),
/// as said, how much, and its share of the window.
data class ChatContextPart(val kind: String, val label: String, val amount: String, val share: Float, val percent: String)

/// A command the field offers: as typed (`/compact`), what it does, what
/// may follow it (a pick then puts it in the field rather than run it),
/// `here` for one the app does itself ("model": the model menu), what it
/// is ("skill", "command", "prompt") and the heading of its group.
data class ChatCommand(
    val name: String,
    val description: String,
    val args: String?,
    val here: String?,
    val kind: String = "command",
    val group: String = "",
)

/// The model button: what it says, and what it opens on -- the models,
/// each opening on how hard it can think.
data class ChatModelMenu(
    val label: String,
    val modelsHeading: String,
    val models: List<ChatModelChoice>,
    val effortsHeading: String,
)

/// A model; `efforts` empty for one picked as it is.
data class ChatModelChoice(val id: String, val label: String, val current: Boolean, val efforts: List<ChatOption>)

data class ChatOption(val id: String, val label: String, val current: Boolean)

/// A picture or file sent with what was said: the key of its small
/// picture (`TerminalModel.chatThumbs`) when the host made one, and where
/// it is on the host.
data class ChatSent(val name: String, val image: Boolean, val picture: String?, val path: String?)

sealed class ChatRow {
    abstract val id: String

    data class User(override val id: String, val text: String, val attachments: List<ChatSent>) : ChatRow()
    /// `copy`: the words as written, for the last reply of a turn.
    data class Text(override val id: String, val blocks: List<MdBlock>, val copy: String? = null) : ChatRow()
    data class Fold(
        override val id: String,
        /// "think", "look", "run", "edit", "tool".
        val icon: String,
        val label: String,
        /// "running", "done", "failed", "declined", "stopped".
        val state: String,
        val badge: String?,
        val added: Int?,
        val removed: Int?,
        val open: Boolean,
        /// There is something to show when it opens; `body` is that,
        /// sent only while it is open.
        val canOpen: Boolean,
        val body: FoldBody?,
    ) : ChatRow()

    /// What the agent did between two things said, as one line ("Ran 3
    /// commands") that opens on its folds; `rows` only while open.
    data class Group(
        override val id: String,
        val icon: String,
        val label: String,
        /// "running", "stopped", "done"; for one thing done, its own
        /// ("failed", "declined").
        val state: String,
        val badge: String?,
        /// For one change done, the lines it added and took out.
        val added: Int?,
        val removed: Int?,
        val open: Boolean,
        val rows: List<Fold>,
    ) : ChatRow()
    data class Note(override val id: String, val text: String) : ChatRow()
}

sealed class FoldBody {
    data class Blocks(val blocks: List<MdBlock>) : FoldBody()
    data class Output(val text: String) : FoldBody()
    data class Listing(val lines: List<String>) : FoldBody()
    data class Diff(val files: List<DiffFile>) : FoldBody()
}

data class DiffFile(val path: String, val lines: List<DiffLine>)
/// `t`: "add", "del", "ctx" or "hunk".
data class DiffLine(val t: String, val s: String)

sealed class MdBlock {
    data class Para(val spans: List<MdSpan>) : MdBlock()
    data class Heading(val level: Int, val spans: List<MdSpan>) : MdBlock()
    data class Item(val depth: Int, val mark: String, val spans: List<MdSpan>) : MdBlock()
    data class Quote(val spans: List<MdSpan>) : MdBlock()
    data class Code(val lang: String, val text: String) : MdBlock()
    object Rule : MdBlock()
    /// How each column lines up ("left", "center", "right", "none"), the
    /// header's cells, then the rows'.
    data class Table(val align: List<String>, val head: List<List<MdSpan>>, val rows: List<List<List<MdSpan>>>) : MdBlock()
}

/// `fade`: how much of its colour it shows, for words just come in.
data class MdSpan(val text: String, val bold: Boolean, val italic: Boolean, val code: Boolean, val strike: Boolean, val link: String?, val fade: Float = 1f)

data class ChatAsk(
    val id: String,
    val label: String,
    val question: String,
    val body: String,
    val reason: String?,
    val place: String?,
    val choices: List<ChatChoice>,
    /// With no choices: the label of the way to the terminal, where the
    /// agent takes its answer.
    val terminal: String?,
)

data class ChatChoice(val id: String, val label: String, val lead: Boolean)
data class ChatProblem(val text: String, val detail: String, val action: ChatAction?)
data class ChatAction(val id: String, val label: String)
/// `kept`: what was typed and not sent, back in the field to be put right.
data class ChatNotice(val seq: Long, val text: String, val kept: String? = null)

object ChatViews {
    fun parse(json: String): ChatView? = runCatching {
        val o = JSONObject(json)
        ChatView(
            pane = o.optInt("pane", -1),
            agent = o.optString("agent"),
            title = o.optString("title"),
            state = o.optString("state"),
            stateLabel = o.optString("state_label"),
            since = o.optLongOrNull("since"),
            busy = o.optStringOrNull("busy"),
            guessed = o.optStringOrNull("guessed"),
            rows = o.optJSONArray("rows").objects().mapNotNull(::row),
            more = o.optBoolean("more"),
            moreLabel = o.optString("more_label"),
            loadingOlder = o.optBoolean("loading_older"),
            ask = o.optJSONObject("ask")?.let { a ->
                ChatAsk(
                    id = a.optString("id"),
                    label = a.optString("label"),
                    question = a.optString("question"),
                    body = a.optString("body"),
                    reason = a.optStringOrNull("reason"),
                    place = a.optStringOrNull("place"),
                    choices = a.optJSONArray("choices").objects().map {
                        ChatChoice(it.optString("id"), it.optString("label"), it.optBoolean("lead"))
                    },
                    terminal = a.optStringOrNull("terminal"),
                )
            },
            problem = o.optJSONObject("problem")?.let { p ->
                ChatProblem(
                    text = p.optString("text"),
                    detail = p.optString("detail"),
                    action = p.optJSONObject("action")?.let { ChatAction(it.optString("id"), it.optString("label")) },
                )
            },
            notice = o.optJSONObject("notice")?.let { ChatNotice(it.optLong("seq"), it.optString("text"), it.optStringOrNull("kept")) },
            placeholder = o.optString("placeholder"),
            canSend = o.optBoolean("can_send"),
            canStop = o.optBoolean("can_stop"),
            canAttach = o.optBoolean("can_attach"),
            model = o.optJSONObject("model")?.let { m ->
                ChatModelMenu(
                    label = m.optString("label"),
                    modelsHeading = m.optString("models_heading"),
                    models = m.optJSONArray("models").objects().map { c ->
                        ChatModelChoice(
                            c.optString("id"),
                            c.optString("label"),
                            c.optBoolean("current"),
                            c.optJSONArray("efforts").objects().map {
                                ChatOption(it.optString("id"), it.optString("label"), it.optBoolean("current"))
                            },
                        )
                    },
                    effortsHeading = m.optString("efforts_heading"),
                )
            },
            commands = o.optJSONArray("commands").objects().map {
                ChatCommand(
                    it.optString("name"),
                    it.optString("description"),
                    it.optStringOrNull("args"),
                    it.optStringOrNull("here"),
                    // An older core says no kind: the name tells a skill.
                    it.optStringOrNull("kind") ?: if (it.optString("name").let { n -> n.startsWith("$") || n.startsWith("/skill:") }) "skill" else "command",
                    it.optString("group"),
                )
            },
            outdated = o.optJSONObject("outdated")?.let { u ->
                u.optJSONObject("action")?.let { a -> ChatOutdated(u.optString("text"), ChatAction(a.optString("id"), a.optString("label"))) }
            },
            times = o.optJSONObject("times")?.let { t -> t.keys().asSequence().associateWith { t.optLong(it) } }.orEmpty(),
            writing = o.optJSONArray("writing").strings().toSet(),
            context = o.optJSONObject("context")?.let { c ->
                ChatContext(
                    fill = if (c.has("fill") && !c.isNull("fill")) c.optDouble("fill").toFloat() else null,
                    level = c.optString("level"),
                    percent = c.optString("percent"),
                    heading = c.optString("heading"),
                    amount = c.optString("amount"),
                    parts = c.optJSONArray("parts").objects().map {
                        ChatContextPart(it.optString("kind"), it.optString("label"), it.optString("amount"), it.optDouble("share").toFloat(), it.optString("percent"))
                    },
                    breakdown = c.optString("breakdown"),
                    compactsAt = c.optStringOrNull("compacts_at"),
                    note = c.optStringOrNull("note"),
                    totalsHeading = c.optString("totals_heading"),
                    totals = c.optJSONArray("totals").objects().map { it.optString("label") to it.optString("value") },
                    compact = c.optStringOrNull("compact"),
                )
            },
        )
    }.getOrNull()

    private fun row(r: JSONObject): ChatRow? {
        val id = r.optString("id")
        return when (r.optString("kind")) {
            "user" -> ChatRow.User(
                id,
                r.optString("text"),
                r.optJSONArray("attachments").objects().map {
                    ChatSent(it.optString("name"), it.optBoolean("image"), it.optStringOrNull("picture"), it.optStringOrNull("path"))
                },
            )
            "text" -> ChatRow.Text(id, blocks(r.optJSONArray("blocks")), r.optStringOrNull("copy"))
            "fold" -> ChatRow.Fold(
                id = id,
                icon = r.optString("icon"),
                label = r.optString("label"),
                state = r.optString("state"),
                badge = r.optStringOrNull("badge"),
                added = r.optIntOrNull("added"),
                removed = r.optIntOrNull("removed"),
                open = r.optBoolean("open"),
                canOpen = r.optBoolean("can_open"),
                body = r.optJSONObject("body")?.let(::body),
            )
            "note" -> ChatRow.Note(id, r.optString("text"))
            "group" -> ChatRow.Group(
                id = id,
                icon = r.optString("icon"),
                label = r.optString("label"),
                state = r.optString("state"),
                badge = r.optStringOrNull("badge"),
                added = r.optIntOrNull("added"),
                removed = r.optIntOrNull("removed"),
                open = r.optBoolean("open"),
                rows = r.optJSONArray("rows").objects().mapNotNull { row(it) as? ChatRow.Fold },
            )
            else -> null
        }
    }

    private fun body(b: JSONObject): FoldBody? = when (b.optString("t")) {
        "text" -> FoldBody.Blocks(blocks(b.optJSONArray("blocks")))
        "output" -> FoldBody.Output(b.optString("text"))
        "list" -> FoldBody.Listing(b.optJSONArray("lines").strings())
        "diff" -> FoldBody.Diff(
            b.optJSONArray("files").objects().map { f ->
                DiffFile(f.optString("path"), f.optJSONArray("lines").objects().map { DiffLine(it.optString("t"), it.optString("s")) })
            }
        )
        else -> null
    }

    fun blocks(array: JSONArray?): List<MdBlock> = array.objects().mapNotNull { b ->
        val spans = { spans(b.optJSONArray("spans")) }
        when (b.optString("t")) {
            "para" -> MdBlock.Para(spans())
            "heading" -> MdBlock.Heading(b.optInt("level", 1), spans())
            "item" -> MdBlock.Item(b.optInt("depth"), b.optString("mark"), spans())
            "quote" -> MdBlock.Quote(spans())
            "code" -> MdBlock.Code(b.optString("lang"), b.optString("text"))
            "rule" -> MdBlock.Rule
            "table" -> MdBlock.Table(
                align = b.optJSONArray("align").strings(),
                head = cells(b.optJSONArray("head")),
                rows = (0 until (b.optJSONArray("rows")?.length() ?: 0)).map { cells(b.optJSONArray("rows")?.optJSONArray(it)) },
            )
            else -> null
        }
    }

    private fun cells(array: JSONArray?): List<List<MdSpan>> =
        if (array == null) emptyList() else (0 until array.length()).map { spans(array.optJSONArray(it)) }

    private fun spans(array: JSONArray?): List<MdSpan> = array.objects().map {
        MdSpan(
            text = it.optString("text"),
            bold = it.optBoolean("bold"),
            italic = it.optBoolean("italic"),
            code = it.optBoolean("code"),
            strike = it.optBoolean("strike"),
            link = it.optStringOrNull("link"),
        )
    }

    private fun JSONArray?.objects(): List<JSONObject> =
        if (this == null) emptyList() else (0 until length()).mapNotNull { optJSONObject(it) }

    private fun JSONArray?.strings(): List<String> =
        if (this == null) emptyList() else (0 until length()).map { optString(it) }

    private fun JSONObject.optStringOrNull(key: String): String? =
        if (has(key) && !isNull(key)) optString(key) else null

    private fun JSONObject.optLongOrNull(key: String): Long? =
        if (has(key) && !isNull(key)) optLong(key) else null

    private fun JSONObject.optIntOrNull(key: String): Int? =
        if (has(key) && !isNull(key)) optInt(key) else null
}

// MARK: the page

/// `base`, its text `scale` times as large: sp converted here still goes
/// through `base`'s own conversion, which grows a large system font size
/// less than in proportion, as the rest of the app does. At the standard
/// size `base` is used as it is.
private class ScaledText(private val base: Density, private val scale: Float) : Density by base {
    // Text laid out from the scale itself (as Compose's paragraphs are)
    // grows by it too.
    override val fontScale: Float get() = base.fontScale * scale

    override fun TextUnit.toDp(): Dp = if (isSp) with(base) { (value * scale).sp.toDp() } else with(base) { this@toDp.toDp() }

    override fun TextUnit.toPx(): Float = if (isSp) with(base) { (value * scale).sp.toPx() } else with(base) { this@toPx.toPx() }

    override fun Dp.toSp(): TextUnit = with(base) { (this@toSp.toSp().value / scale).sp }

    override fun Float.toSp(): TextUnit = with(base) { (this@toSp.toSp().value / scale).sp }
}

/// How large the chat's text is drawn, as its setting says.
internal fun chatTextScale(setting: String): Float = when (setting) {
    "s" -> 0.9f
    "l" -> 1.12f
    "xl" -> 1.25f
    else -> 1f
}

/// Further into the last row than any row is tall: a scroll to it ends at
/// the end of the list.
private const val TO_THE_END = 1 shl 20

private val Working = Color(0xFF34C759)
private val Waiting = Color(0xFFFF9F0A)
private val Resting = Color(0xFF8E8E93)
private val Added = Color(0xFF30D158)
private val Removed = Color(0xFFFF6961)

/// The chat with the agent of a pane, over that pane's terminal: what it
/// said and did, what it asks, and a field to say something. Going back
/// is going to its terminal.
@OptIn(androidx.compose.foundation.layout.ExperimentalLayoutApi::class)
@Composable
fun ChatPage(model: TerminalModel, onBack: () -> Unit) {
    val target = model.chatWith ?: return
    val chat = model.chat
    val chrome = LocalChrome.current
    val dark = chrome.dark
    val ground = listBackdrop(dark)
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val backdrop = rememberLayerBackdrop()
    val bars = WindowInsets.statusBars.asPaddingValues()
    val density = LocalDensity.current
    var footPx by remember { mutableIntStateOf(0) }
    // The bar's row (back, and whom the chat is with), as tall as the
    // system's text size makes it.
    var barRow by remember { mutableIntStateOf(with(density) { 38.dp.roundToPx() }) }
    val list = rememberLazyListState()
    val scope = rememberCoroutineScope()
    // The context's sheet, risen from the bottom.
    var showContext by remember(target.pane) { mutableStateOf(false) }
    BackHandler { onBack() }

    // A reply on its way shows as it is written, its words coming in at a
    // steady pace; or, set to blocks, only once written whole.
    val streaming = model.settings.chatReveal != "blocks"
    val still = rememberReducedMotion()
    val reveal = remember(target.pane) { Reveal() }
    // Opened at the end, and kept there while the person reads the end:
    // what comes next is in view as it comes.
    val rows = chat?.rows.orEmpty().let { all ->
        if (streaming) all else all.filter { it !is ChatRow.Text || it.id !in chat?.writing.orEmpty() }
    }
    val last = rows.lastOrNull()
    var opened by remember(target.pane) { mutableStateOf(false) }
    // Just said something: what comes of it is shown, wherever the person was.
    var follow by remember(target.pane) { mutableStateOf(false) }
    // The row on top when older ones were asked for: it stays in its place.
    var keep by remember(target.pane) { mutableStateOf<String?>(null) }
    // Whether the person reads the end: as their last scroll (or the last
    // one made here) left the list. What comes in below does not change
    // it: rows that came in a burst once read as a scroll away from it.
    var reading by remember(target.pane) { mutableStateOf(true) }
    LaunchedEffect(list, target.pane) {
        snapshotFlow { list.isScrollInProgress }.collect { scrolling ->
            if (!scrolling) reading = !list.canScrollForward
        }
    }
    // Also when what is under the list grows (a question, the keyboard):
    // the end stays above it.
    LaunchedEffect(rows.size, last, chat?.busy, chat?.loadingOlder, footPx) {
        if (rows.isEmpty()) return@LaunchedEffect
        // The list's items as composed below: what goes before the rows,
        // the rows, and the line after them.
        val before = (if (chat?.more == true) 1 else 0) + (if (chat?.guessed != null) 1 else 0)
        keep?.let { id ->
            val at = rows.indexOfFirst { it.id == id }
            if (at > 0) {
                keep = null
                list.scrollToItem(before + at)
                return@LaunchedEffect
            }
            if (chat?.loadingOlder != true) keep = null
        }
        // A scroll on its way (a fling up from the end) is the person's: what
        // comes in meanwhile does not pull the list back.
        val atEnd = !opened || follow || (reading && !list.isScrollInProgress)
        opened = true
        if (atEnd && last != null) {
            follow = false
            // To the end of the last row, however much taller than the
            // screen it is: the list stops where its content does.
            list.scrollToItem(before + rows.size - 1 + (if (chat?.busy != null) 1 else 0), TO_THE_END)
        }
    }
    // The rows on show when the chat opened come as they are; later ones
    // come in.
    LaunchedEffect(target.pane, rows.isNotEmpty()) {
        if (rows.isNotEmpty()) reveal.prime(rows)
    }
    // What the core let go of (past what it keeps, another session) is
    // let go here too.
    LaunchedEffect(rows) { reveal.prune(rows.mapTo(HashSet()) { it.id }) }
    // Replies on their way catch up with what has come of them, a little
    // each frame, the list following the end as they grow when the person
    // reads there. It rests once all have caught up, until more comes.
    val texts = rows.filterIsInstance<ChatRow.Text>()
    val wanted = texts.sumOf { reveal.lengthOf(it) }
    val writingNow by rememberUpdatedState(chat?.writing.orEmpty())
    val textsNow by rememberUpdatedState(texts)
    val toEnd by rememberUpdatedState(suspend {
        if (reading && !list.isScrollInProgress && rows.isNotEmpty()) {
            val before = (if (chat?.more == true) 1 else 0) + (if (chat?.guessed != null) 1 else 0)
            list.scrollToItem(before + rows.size - 1 + (if (chat?.busy != null) 1 else 0), TO_THE_END)
        }
    })
    LaunchedEffect(target.pane, wanted, streaming, still) {
        if (!streaming || still) return@LaunchedEffect
        var last = 0L
        while (true) {
            val moved = withFrameNanos { now ->
                val seconds = if (last == 0L) 1f / 60 else ((now - last) / 1e9f).coerceIn(0f, 0.1f)
                last = now
                reveal.step(textsNow, writingNow, seconds)
            }
            if (!moved) break
            toEnd()
        }
    }
    // Swiped left: the rows move aside for when each came about.
    var swipe by remember { mutableFloatStateOf(0f) }
    val gutterPx = with(density) { TIME_GUTTER.toPx() }

    Box(Modifier.fillMaxSize().background(ground)) {
        // Whatever no part of the page takes stays here: under the page is
        // the terminal, which a tap would focus and raise its keyboard for.
        Box(
            Modifier.matchParentSize().pointerInput(Unit) {
                awaitPointerEventScope {
                    while (true) awaitPointerEvent().changes.forEach { it.consume() }
                }
            }
        )
        // The chat's text as large as its setting makes it: what was said,
        // and what was done; the bars keep theirs.
        val textScale = chatTextScale(model.settings.chatText)
        val scaled = remember(density, textScale) { if (textScale == 1f) density else ScaledText(density, textScale) }
        CompositionLocalProvider(LocalDensity provides scaled) {
        LazyColumn(
            state = list,
            modifier = Modifier
                .fillMaxSize()
                .layerBackdrop(backdrop)
                .background(ground)
                .pointerInput(target.pane) {
                    detectHorizontalDragGestures(
                        onDragEnd = { scope.launch { animate(swipe, 0f, animationSpec = spring(dampingRatio = 0.8f, stiffness = 500f)) { at, _ -> swipe = at } } },
                        onDragCancel = { swipe = 0f },
                    ) { change, dx ->
                        change.consume()
                        // Past the times, it gives less and less.
                        val next = swipe + dx
                        swipe = if (next < -gutterPx) -gutterPx + (next + gutterPx) / 3 else next.coerceAtMost(0f)
                    }
                },
            contentPadding = PaddingValues(
                start = 16.dp,
                end = 16.dp,
                top = bars.calculateTopPadding() + 24.dp + with(density) { barRow.toDp() },
                bottom = with(density) { footPx.toDp() } + 16.dp,
            ),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            if (chat?.more == true) {
                item(key = "tt:more") {
                    Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.Center) {
                        if (chat.loadingOlder) {
                            CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp, color = secondary)
                        } else {
                            // Not glass: the list is the backdrop, and glass in
                            // it would draw itself inside itself without end.
                            Text(
                                chat.moreLabel,
                                fontSize = 13.sp,
                                color = secondary,
                                modifier = Modifier
                                    .clip(CircleShape)
                                    .background(if (dark) Color(0xFF2C2C2E) else Color(0xFFE9E9EB))
                                    .clickable {
                                        keep = rows.firstOrNull()?.id
                                        model.chatOlder()
                                    }
                                    .padding(horizontal = 14.dp, vertical = 7.dp),
                            )
                        }
                    }
                }
            }
            chat?.guessed?.let { text ->
                item(key = "tt:guessed") {
                    Text(text, fontSize = 12.5.sp, color = secondary, textAlign = TextAlign.Center, modifier = Modifier.fillMaxWidth())
                }
            }
            if (chat == null || (rows.isEmpty() && chat.state == "connecting")) {
                item(key = "tt:connecting") {
                    Box(Modifier.fillMaxWidth().padding(top = 80.dp), contentAlignment = Alignment.Center) {
                        CircularProgressIndicator(Modifier.size(22.dp), strokeWidth = 2.dp, color = secondary)
                    }
                }
            }
            // Nothing said yet: whom the chat is with, where the words will be.
            if (chat != null && rows.isEmpty() && chat.state != "connecting" && chat.problem == null && !chat.more) {
                item(key = "tt:empty") {
                    Column(
                        Modifier.fillParentMaxWidth().fillParentMaxHeight(0.5f).animateItem(fadeInSpec = tween(300), fadeOutSpec = null),
                        verticalArrangement = Arrangement.Bottom,
                        horizontalAlignment = Alignment.CenterHorizontally,
                    ) {
                        Box(
                            Modifier.size(52.dp).clip(CircleShape).background(chrome.ink.copy(alpha = 0.06f)),
                            contentAlignment = Alignment.Center,
                        ) {
                            DesktopIcon("message-circle", size = 24.dp, tint = secondary)
                        }
                        Spacer(Modifier.height(12.dp))
                        Text(chat.agent.ifEmpty { target.agent }, fontSize = 18.sp, fontWeight = FontWeight.SemiBold, color = chrome.ink)
                        Text(target.place, fontSize = 13.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    }
                }
            }
            // Rows come in fading. None slides as the one above it grows (a
            // reply being written): the list stays still under the words.
            // One that goes is gone at once (a fading one was left behind
            // on screen).
            items(rows, key = { it.id }) { row ->
                val fresh = reveal.fresh(row.id, rows)
                Entering(fresh && !still) {
                    TimedRow(chat?.times?.get(row.id), swipe, gutterPx) {
                        val shown = (row as? ChatRow.Text)?.let { reveal.shownOf(it, chat?.writing.orEmpty(), streaming && !still) }
                        ChatRowView(row, model, dark, shown)
                    }
                }
            }
            chat?.busy?.let { busy ->
                item(key = "tt:busy") {
                    Box(Modifier.animateItem(fadeInSpec = tween(220), placementSpec = null, fadeOutSpec = null)) { Busy(busy, chat.since, secondary) }
                }
            }
        }
        }

        // The bar: back, and whom the chat is with. Its two lines reach
        // far down it: what scrolls under them is covered, and fades in
        // only below them, where the list begins -- however tall the
        // system's text size makes them.
        Box(
            Modifier
                .fillMaxWidth()
                // Nearly all ground under its words: no line passing under
                // them reads as a smudge beside them.
                .progressiveBlur(backdrop, ground, top = true, solid = bars.calculateTopPadding() + 6.dp + with(density) { barRow.toDp() }, cover = 0.94f)
                .padding(start = 12.dp, end = 12.dp, top = 6.dp + bars.calculateTopPadding(), bottom = 18.dp),
        ) {
            Row(
                Modifier.onSizeChanged { barRow = it.height },
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                val back = tr("back")
                Box(
                    Modifier
                        .size(40.dp)
                        .glass(backdrop, dark, CircleShape)
                        .clickable(role = Role.Button, onClick = onBack)
                        .semantics { contentDescription = back },
                    contentAlignment = Alignment.Center,
                ) {
                    DesktopIcon("chevron-left", size = 20.dp, tint = chrome.ink)
                }
                Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(1.dp)) {
                    Text(target.place, fontSize = 16.sp, lineHeight = 20.sp, fontWeight = FontWeight.SemiBold, color = chrome.ink, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        val dot = when (chat?.state) {
                            "working" -> Working
                            "blocked" -> Waiting
                            else -> Resting
                        }
                        Box(Modifier.size(7.dp).clip(CircleShape).background(dot))
                        val who = chat?.agent?.takeIf { it.isNotEmpty() } ?: target.agent
                        val said = listOfNotNull(who, chat?.stateLabel?.takeIf { it.isNotEmpty() }, chat?.title?.takeIf { it.isNotEmpty() })
                        Text(said.joinToString(" · "), fontSize = 12.5.sp, lineHeight = 16.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    }
                }
                chat?.context?.let { context -> ContextRing(context, backdrop, dark) { showContext = true } }
            }
        }

        // Scrolled up to read: a way back down to what comes now.
        Box(
            Modifier
                .align(Alignment.BottomCenter)
                .imePadding()
                .navigationBarsPadding()
                .padding(bottom = with(density) { footPx.toDp() } + 10.dp),
        ) {
            val down = tr("chat.latest")
            AnimatedVisibility(
                visible = rows.isNotEmpty() && list.canScrollForward,
                enter = fadeIn(tween(160)) + scaleIn(tween(160), initialScale = 0.8f),
                exit = fadeOut(tween(120)) + scaleOut(tween(120), targetScale = 0.8f),
            ) {
                Box(
                    Modifier
                        .size(40.dp)
                        .glass(backdrop, dark, CircleShape)
                        .clickable(role = Role.Button) {
                            scope.launch { list.animateScrollToItem((list.layoutInfo.totalItemsCount - 1).coerceAtLeast(0), TO_THE_END) }
                        }
                        .semantics { contentDescription = down },
                    contentAlignment = Alignment.Center,
                ) {
                    DesktopIcon("arrow-down", size = 18.dp, tint = chrome.ink)
                }
            }
        }

        val context = chat?.context
        if (showContext && context != null) {
            ContextSheet(context, dark, onDismiss = { showContext = false }) {
                showContext = false
                model.chatCommand("/compact")
            }
        }

        val keyboardUp = WindowInsets.isImeVisible
    val footClearance = cornerClearance(side = 10.dp, radius = 24.dp, bottom = 8.dp + WindowInsets.navigationBars.asPaddingValues().calculateBottomPadding())
    // What it asks, or what is wrong, over the field to answer in.
        // What scrolls under them blurs and fades into the ground toward
        // the screen's edge, as under iOS's bars: never read through them.
        Column(
            Modifier
                .align(Alignment.BottomCenter)
                .fillMaxWidth()
                .progressiveBlur(backdrop, ground, top = false, cover = 0.94f)
                .imePadding()
                .navigationBarsPadding()
                .onSizeChanged { footPx = it.height }
                // Clear of a large rounded screen corner, the keyboard down.
                .padding(start = 10.dp, end = 10.dp, top = 14.dp, bottom = 8.dp + if (keyboardUp) 0.dp else footClearance),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            // A chat's notices are numbered from one again: the count of
            // the ones shown is the chat's own.
            key(target.pane) { Notice(chat?.notice, backdrop, dark) }
            // Cards over the field rise in and drop away, keeping what they
            // said while they go.
            Appearing(target.pane, chat?.problem) { ProblemCard(it, model, backdrop, dark) }
            Appearing(target.pane, chat?.outdated) { OutdatedCard(it, model, backdrop, dark) }
            Appearing(target.pane, chat?.ask) { AskCard(it, model, backdrop, dark, onTerminal = { model.closeChat(toTerminal = true) }) }
            ChatField(chat, target, model, backdrop, dark, onSend = { follow = true })
        }
    }
}

@Composable
private fun ChatRowView(row: ChatRow, model: TerminalModel, dark: Boolean, shown: Int? = null) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    when (row) {
        is ChatRow.User -> Column(
            Modifier.fillMaxWidth(),
            horizontalAlignment = Alignment.End,
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            if (row.attachments.isNotEmpty()) SentFiles(row.attachments, model, dark)
            if (row.text.isNotEmpty()) {
                SelectionContainer {
                    Text(
                        row.text,
                        fontSize = 16.sp,
                        lineHeight = 23.sp,
                        color = ink,
                        modifier = Modifier
                            .widthIn(max = 320.dp)
                            .clip(RoundedCornerShape(20.dp, 20.dp, 6.dp, 20.dp))
                            .background(if (dark) Color(0xFF2C2C2E) else Color(0xFFE9E9EB))
                            .padding(horizontal = 14.dp, vertical = 9.dp),
                    )
                }
            }
        }
        // Grows as more of what it says comes, at once: an animated growth
        // a few times a second kept the list moving under the words.
        is ChatRow.Text -> Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
            // As far as it has come in: the newest words fading in.
            val blocks = if (shown != null) remember(row.blocks, shown) { mdHead(row.blocks, shown) } else row.blocks
            SelectionContainer { MdBlocks(blocks, ink, dark, onCopy = model::copyQuietly, whole = row.blocks) }
            // The answer whole, as written.
            row.copy?.let { words -> CopyButton(secondary, size = 15.dp) { model.copyQuietly(words) } }
        }
        is ChatRow.Fold -> FoldView(row, model, dark, secondary)
        is ChatRow.Group -> GroupView(row, model, dark, secondary)
        is ChatRow.Note -> Text(
            row.text,
            fontSize = 12.5.sp,
            color = secondary,
            textAlign = TextAlign.Center,
            modifier = Modifier.fillMaxWidth(),
        )
    }
}

@Composable
private fun FoldView(row: ChatRow.Fold, model: TerminalModel, dark: Boolean, secondary: Color) {
    // Turned at the tap, before the core's view comes back with what opens.
    val open = rememberOpen(row.id, row.open)
    Column(Modifier.fillMaxWidth()) {
        Row(
            Modifier
                .fillMaxWidth()
                .heightIn(min = 28.dp)
                .clip(RoundedCornerShape(8.dp))
                .clickable(enabled = row.canOpen) {
                    open.value = !open.value
                    model.chatToggle(row.id)
                }
                .padding(vertical = 3.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(7.dp),
        ) {
            if (row.state == "running") {
                CircularProgressIndicator(Modifier.size(12.dp), strokeWidth = 1.5.dp, color = secondary)
            } else {
                Chevron(open.value, secondary)
            }
            DesktopIcon(foldIcon(row.icon), size = 15.dp, tint = secondary)
            Text(
                row.label,
                fontSize = 14.sp,
                color = secondary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            row.added?.takeIf { it > 0 }?.let { Text("+$it", fontSize = 13.sp, color = Added) }
            row.removed?.takeIf { it > 0 }?.let { Text("−$it", fontSize = 13.sp, color = Removed) }
            when (row.state) {
                "failed" -> Text(row.badge ?: "✕", fontSize = 12.5.sp, color = Removed)
                "declined" -> Text(row.badge ?: "", fontSize = 12.5.sp, color = Waiting)
                "done" -> if (row.icon == "run") DesktopIcon("check", size = 13.dp, tint = Added)
            }
        }
        // What opens slides open and fades in, and closes the same way: the
        // last body shown stays on show while it goes.
        var shown by remember(row.id) { mutableStateOf(row.body) }
        if (row.body != null) shown = row.body
        AnimatedVisibility(
            visible = open.value && row.open && row.body != null,
            enter = expandVertically(tween(220)) + fadeIn(tween(220)),
            exit = shrinkVertically(tween(180)) + fadeOut(tween(140)),
        ) {
            shown?.let { body -> Box(Modifier.padding(start = 20.dp, top = 4.dp)) { FoldBodyView(body, dark, secondary) } }
        }
    }
}

/// `value`'s card, rising in as it comes and dropping away as it goes, with
/// what it last said -- the chat's own (`chat`), and not to be tapped once
/// it is going.
@Composable
private fun <T : Any> Appearing(chat: Int, value: T?, content: @Composable (T) -> Unit) {
    var last by remember(chat) { mutableStateOf(value) }
    if (value != null) last = value
    AnimatedVisibility(
        visible = value != null,
        enter = fadeIn(tween(200)) + slideInVertically(tween(220)) { it / 4 } + expandVertically(tween(220), expandFrom = Alignment.Bottom),
        exit = fadeOut(tween(150)) + shrinkVertically(tween(200), shrinkTowards = Alignment.Bottom),
    ) {
        val going = value == null
        Box(
            if (!going) Modifier else Modifier.pointerInput(Unit) {
                awaitPointerEventScope {
                    while (true) awaitPointerEvent(PointerEventPass.Initial).changes.forEach { it.consume() }
                }
            },
        ) {
            last?.let { content(it) }
        }
    }
}

/// Whether a row is open, as last tapped: turned at once, and following
/// the core's view when that says otherwise (another of the person's
/// devices, a fold opened by the core itself).
@Composable
private fun rememberOpen(id: String, open: Boolean): androidx.compose.runtime.MutableState<Boolean> {
    val state = remember(id) { mutableStateOf(open) }
    LaunchedEffect(open) { state.value = open }
    return state
}

/// The fold's chevron: right when shut, turning down as it opens.
@Composable
private fun Chevron(open: Boolean, tint: Color) {
    val turn by animateFloatAsState(if (open) 90f else 0f, tween(200), label = "chevron")
    DesktopIcon("chevron-right", size = 13.dp, tint = tint, modifier = Modifier.graphicsLayer { rotationZ = turn })
}

/// Work done between two things said, as one line; open, its folds.
@Composable
private fun GroupView(row: ChatRow.Group, model: TerminalModel, dark: Boolean, secondary: Color) {
    val open = rememberOpen(row.id, row.open)
    Column(Modifier.fillMaxWidth()) {
        Row(
            Modifier
                .fillMaxWidth()
                .heightIn(min = 28.dp)
                .clip(RoundedCornerShape(8.dp))
                .clickable {
                    open.value = !open.value
                    model.chatToggle(row.id)
                }
                .padding(vertical = 3.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(7.dp),
        ) {
            if (row.state == "running") {
                CircularProgressIndicator(Modifier.size(12.dp), strokeWidth = 1.5.dp, color = secondary)
            } else {
                Chevron(open.value, secondary)
            }
            DesktopIcon(foldIcon(row.icon), size = 15.dp, tint = secondary)
            Text(
                row.label,
                fontSize = 14.sp,
                color = secondary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            row.added?.takeIf { it > 0 }?.let { Text("+$it", fontSize = 13.sp, color = Added) }
            row.removed?.takeIf { it > 0 }?.let { Text("−$it", fontSize = 13.sp, color = Removed) }
            row.badge?.let { Text(it, fontSize = 12.5.sp, color = if (row.state == "declined") Waiting else Removed) }
        }
        var shown by remember(row.id) { mutableStateOf(row.rows) }
        if (row.rows.isNotEmpty()) shown = row.rows
        AnimatedVisibility(
            visible = open.value && row.open && row.rows.isNotEmpty(),
            enter = expandVertically(tween(220)) + fadeIn(tween(220)),
            exit = shrinkVertically(tween(180)) + fadeOut(tween(140)),
        ) {
            Column(
                Modifier.padding(start = 20.dp, top = 2.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                for (fold in shown) key(fold.id) { FoldView(fold, model, dark, secondary) }
            }
        }
    }
}

private fun foldIcon(icon: String) = when (icon) {
    "think" -> "lightbulb"
    "look" -> "file-text"
    "run" -> "terminal"
    "edit" -> "pencil"
    "group" -> "layers"
    else -> "wrench"
}

@Composable
private fun FoldBodyView(body: FoldBody, dark: Boolean, secondary: Color) {
    val mono = monoFamily()
    val codeGround = if (dark) Color(0xFF17181B) else Color(0xFFF2F2F5)
    when (body) {
        is FoldBody.Blocks -> MdBlocks(body.blocks, secondary, dark)
        is FoldBody.Output -> Box(
            Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(12.dp))
                .background(codeGround)
                .horizontalScroll(rememberScrollState())
                .padding(horizontal = 12.dp, vertical = 10.dp),
        ) {
            Text(body.text, fontFamily = mono, fontSize = 12.sp, lineHeight = 17.sp, color = secondary, softWrap = false)
        }
        is FoldBody.Listing -> Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
            for (line in body.lines) Text(line, fontFamily = mono, fontSize = 12.5.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
        is FoldBody.Diff -> Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            for (file in body.files) {
                Column(
                    Modifier
                        .fillMaxWidth()
                        .clip(RoundedCornerShape(12.dp))
                        .background(codeGround),
                ) {
                    Text(file.path, fontFamily = mono, fontSize = 11.5.sp, color = secondary, modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp))
                    // As wide as its widest line, and the box at least: a
                    // line's colour goes all the way across.
                    BoxWithConstraints {
                        val across = maxWidth
                        Column(Modifier.horizontalScroll(rememberScrollState()).widthIn(min = across).width(IntrinsicSize.Max)) {
                            for (line in file.lines) {
                                val (bg, fg, mark) = when (line.t) {
                                    "add" -> Triple(Added.copy(alpha = 0.14f), if (dark) Color(0xFFBFF7CB) else Color(0xFF14692E), "+")
                                    "del" -> Triple(Removed.copy(alpha = 0.14f), if (dark) Color(0xFFFFC4BF) else Color(0xFF9F1D17), "-")
                                    "hunk" -> Triple(Color.Transparent, Color(0xFF7F8BFF), "")
                                    else -> Triple(Color.Transparent, secondary, " ")
                                }
                                Text(
                                    if (line.t == "hunk") line.s else "$mark ${line.s}",
                                    fontFamily = mono,
                                    fontSize = 11.5.sp,
                                    lineHeight = 17.sp,
                                    color = fg,
                                    softWrap = false,
                                    modifier = Modifier.fillMaxWidth().background(bg).padding(horizontal = 10.dp),
                                )
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Markdown's blocks as text views.
@Composable
/// `whole`: the blocks in full when `blocks` is only as far as they have
/// come in; a code block's copy takes its code from it.
fun MdBlocks(blocks: List<MdBlock>, color: Color, dark: Boolean, onCopy: ((String) -> Unit)? = null, whole: List<MdBlock>? = null) {
    val mono = monoFamily()
    val codeGround = if (dark) Color(0xFF17181B) else Color(0xFFF2F2F5)
    val inlineGround = if (dark) Color(0x1CFFFFFF) else Color(0x10000000)
    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        for ((at, block) in blocks.withIndex()) {
            when (block) {
                is MdBlock.Para -> Text(spans(block.spans, mono, inlineGround, color), fontSize = 16.sp, lineHeight = 24.sp, color = color)
                is MdBlock.Heading -> Text(
                    spans(block.spans, mono, inlineGround, color),
                    fontSize = when (block.level) { 1 -> 20.sp; 2 -> 18.sp; else -> 16.5.sp },
                    lineHeight = when (block.level) { 1 -> 27.sp; 2 -> 25.sp; else -> 23.sp },
                    fontWeight = FontWeight.SemiBold,
                    color = color,
                    modifier = Modifier.padding(top = if (at > 0 && block.level <= 2) 6.dp else 0.dp),
                )
                is MdBlock.Item -> Row(Modifier.padding(start = (block.depth * 18).dp)) {
                    Text(block.mark, fontSize = 16.sp, lineHeight = 24.sp, color = color.copy(alpha = 0.55f), modifier = Modifier.width(22.dp))
                    Text(spans(block.spans, mono, inlineGround, color), fontSize = 16.sp, lineHeight = 24.sp, color = color)
                }
                is MdBlock.Quote -> Row(Modifier.height(IntrinsicSize.Min)) {
                    Box(Modifier.width(3.dp).fillMaxHeight().clip(CircleShape).background(color.copy(alpha = 0.25f)))
                    Text(
                        spans(block.spans, mono, inlineGround, color.copy(alpha = 0.72f)),
                        fontSize = 16.sp,
                        lineHeight = 24.sp,
                        color = color.copy(alpha = 0.72f),
                        modifier = Modifier.padding(start = 12.dp),
                    )
                }
                is MdBlock.Code -> Column(
                    Modifier
                        .fillMaxWidth()
                        .clip(RoundedCornerShape(12.dp))
                        .background(codeGround),
                ) {
                    // Its language, and a way to copy it whole.
                    val headed = block.lang.isNotBlank() || onCopy != null
                    if (headed) {
                        Row(
                            Modifier.fillMaxWidth().padding(start = 12.dp, end = 4.dp, top = 2.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text(block.lang, fontSize = 11.5.sp, color = color.copy(alpha = 0.5f), modifier = Modifier.weight(1f))
                            if (onCopy != null) CopyButton(color.copy(alpha = 0.5f), size = 14.dp) { onCopy((whole?.getOrNull(at) as? MdBlock.Code)?.text ?: block.text) }
                        }
                    }
                    Box(
                        Modifier
                            .fillMaxWidth()
                            .horizontalScroll(rememberScrollState())
                            .padding(start = 12.dp, end = 12.dp, top = if (headed) 0.dp else 10.dp, bottom = 10.dp),
                    ) {
                        Text(block.text, fontFamily = mono, fontSize = 13.sp, lineHeight = 19.sp, color = color, softWrap = false)
                    }
                }
                MdBlock.Rule -> Box(Modifier.fillMaxWidth().height(1.dp).background(color.copy(alpha = 0.15f)))
                is MdBlock.Table -> MdTable(block, color, mono, inlineGround)
            }
        }
    }
}

/// A table as a phone shows one: the header bold, a thin line under each
/// row, each column as wide as its widest cell (a longer one wraps), the
/// whole sliding sideways when wider than the screen.
@Composable
private fun MdTable(table: MdBlock.Table, color: Color, mono: FontFamily, inlineGround: Color) {
    val columns = maxOf(table.head.size, table.rows.maxOfOrNull { it.size } ?: 0)
    if (columns == 0) return
    val all = listOf(table.head) + table.rows
    val widest = with(LocalDensity.current) { 240.dp.roundToPx() }
    val thin = with(LocalDensity.current) { 0.5.dp.toPx() }
    val thick = with(LocalDensity.current) { 1.dp.toPx() }
    Box(Modifier.fillMaxWidth().horizontalScroll(rememberScrollState())) {
        Layout(
            content = {
                for ((r, row) in all.withIndex()) {
                    for (c in 0 until columns) {
                        val lines = when (table.align.getOrNull(c)) {
                            "center" -> TextAlign.Center
                            "right" -> TextAlign.End
                            else -> TextAlign.Start
                        }
                        val last = r == all.size - 1
                        Text(
                            spans(row.getOrNull(c).orEmpty(), mono, inlineGround),
                            fontSize = 14.5.sp,
                            lineHeight = 21.sp,
                            fontWeight = if (r == 0) FontWeight.SemiBold else null,
                            textAlign = lines,
                            color = color,
                            modifier = Modifier
                                .drawBehind {
                                    if (!last) {
                                        val w = if (r == 0) thick else thin
                                        drawRect(
                                            color.copy(alpha = if (r == 0) 0.22f else 0.1f),
                                            topLeft = Offset(0f, size.height - w),
                                            size = Size(size.width, w),
                                        )
                                    }
                                }
                                .padding(horizontal = 10.dp, vertical = 8.dp),
                        )
                    }
                }
            },
        ) { cells, _ ->
            // Each column as wide as its widest cell, up to `widest`; each
            // row as tall as its tallest cell at those widths.
            val widths = IntArray(columns)
            cells.forEachIndexed { i, cell ->
                widths[i % columns] = maxOf(widths[i % columns], minOf(cell.maxIntrinsicWidth(Constraints.Infinity), widest))
            }
            val heights = IntArray(all.size)
            cells.forEachIndexed { i, cell ->
                heights[i / columns] = maxOf(heights[i / columns], cell.minIntrinsicHeight(widths[i % columns]))
            }
            val placed = cells.mapIndexed { i, cell -> cell.measure(Constraints.fixed(widths[i % columns], heights[i / columns])) }
            layout(widths.sum(), heights.sum()) {
                var y = 0
                for (r in all.indices) {
                    var x = 0
                    for (c in 0 until columns) {
                        placed[r * columns + c].place(x, y)
                        x += widths[c]
                    }
                    y += heights[r]
                }
            }
        }
    }
}

private fun spans(spans: List<MdSpan>, mono: FontFamily, inlineGround: Color, color: Color = Color.Unspecified): AnnotatedString = buildAnnotatedString {
    for (span in spans) {
        val style = SpanStyle(
            fontWeight = if (span.bold) FontWeight.SemiBold else null,
            fontStyle = if (span.italic) FontStyle.Italic else null,
            fontFamily = if (span.code) mono else null,
            // Code a little smaller than the words around it, as it reads
            // larger in a fixed width.
            fontSize = if (span.code) 0.88.em else TextUnit.Unspecified,
            background = if (span.code) inlineGround else Color.Unspecified,
            textDecoration = when {
                span.strike -> TextDecoration.LineThrough
                span.link != null -> TextDecoration.Underline
                else -> null
            },
            color = when {
                span.link != null -> LeadBlue.copy(alpha = span.fade)
                // Words just come in, still fading in.
                span.fade < 1f && color.isSpecified -> color.copy(alpha = color.alpha * span.fade)
                else -> Color.Unspecified
            },
        )
        if (span.link != null) {
            withLink(LinkAnnotation.Url(span.link)) { withStyle(style) { append(span.text) } }
        } else {
            withStyle(style) { append(span.text) }
        }
    }
}

@Composable
private fun monoFamily(): FontFamily {
    val context = LocalContext.current
    return remember { runCatching { FontFamily(Font("JetBrainsMono-Regular.ttf", context.assets)) }.getOrDefault(FontFamily.Monospace) }
}

/// While it works: what it is doing, and how long it has been at it.
/// How far a left swipe moves the rows aside, for their times.
private val TIME_GUTTER = 64.dp
/// How many characters at the growing end of a reply fade in.
private const val FADING = 24
/// The slowest a reply comes in, in characters a second; and, faster, all
/// it is behind over this many seconds, again each frame: a burst comes in
/// quickly at first and eases as it catches up.
private const val REVEAL_PACE = 90f
private const val REVEAL_BEHIND = 0.6f

/// How much of each reply on its way is on show, and which rows came after
/// the chat opened (they come in; the rest are as they are).
private class Reveal {
    private val shown = mutableStateMapOf<String, Int>()
    private val carry = HashMap<String, Float>()
    private val lengths = HashMap<String, Pair<List<MdBlock>, Int>>()
    private val known = HashSet<String>()
    /// The rows on show when the chat opened: one being written then shows
    /// as far as it had come.
    private val initial = HashSet<String>()
    private var primed = false

    fun prime(rows: List<ChatRow>) {
        if (primed) return
        primed = true
        known += rows.map { it.id }
        initial += rows.map { it.id }
    }

    /// Where a reply not yet stepped begins: from nothing for one begun
    /// while the chat is open, else whole.
    private fun start(id: String, writing: Set<String>, full: Int): Int =
        if (primed && id in writing && id !in initial) 0 else full

    /// Whether `id` came after the chat opened and has not come in yet: the
    /// newest few rows only (older ones loaded above are as they are).
    fun fresh(id: String, rows: List<ChatRow>): Boolean {
        if (!primed || id in known) return false
        known += id
        return rows.takeLast(3).any { it.id == id }
    }

    fun prune(ids: Set<String>) {
        shown.keys.retainAll(ids)
        carry.keys.retainAll(ids)
        lengths.keys.retainAll(ids)
    }

    fun lengthOf(row: ChatRow.Text): Int {
        val held = lengths[row.id]
        if (held != null && held.first === row.blocks) return held.second
        return mdLength(row.blocks).also { lengths[row.id] = row.blocks to it }
    }

    /// How much of `row` shows: none of one begun after the chat opened
    /// until it comes in; all of one seen whole.
    fun shownOf(row: ChatRow.Text, writing: Set<String>, moving: Boolean): Int? {
        if (!moving) return null
        val full = lengthOf(row)
        val at = shown[row.id] ?: start(row.id, writing, full)
        return at.takeIf { it < full }
    }

    /// One frame on: each reply behind what came of it catches up a little.
    /// False when none is behind.
    fun step(rows: List<ChatRow.Text>, writing: Set<String>, seconds: Float): Boolean {
        var moved = false
        for (row in rows) {
            val full = lengthOf(row)
            val at = shown[row.id] ?: start(row.id, writing, full)
            if (at >= full) {
                if (shown[row.id] != full) shown[row.id] = full
                continue
            }
            val pace = maxOf(REVEAL_PACE, (full - at) / REVEAL_BEHIND)
            val gone = (carry[row.id] ?: 0f) + pace * seconds
            val whole = gone.toInt()
            carry[row.id] = gone - whole
            shown[row.id] = minOf(full, at + whole)
            moved = true
        }
        return moved
    }
}

/// How many characters `blocks` show: what a reply's growth is counted in.
private fun mdLength(blocks: List<MdBlock>): Int = blocks.sumOf { mdLength(it) }

private fun mdLength(block: MdBlock): Int = when (block) {
    is MdBlock.Para -> block.spans.sumOf { it.text.length }
    is MdBlock.Heading -> block.spans.sumOf { it.text.length }
    is MdBlock.Item -> block.spans.sumOf { it.text.length }
    is MdBlock.Quote -> block.spans.sumOf { it.text.length }
    is MdBlock.Code -> block.text.length
    MdBlock.Rule -> 1
    is MdBlock.Table -> (listOf(block.head) + block.rows).sumOf { row -> row.sumOf { cell -> cell.sumOf { it.text.length } } }
}

/// `text` cut at about `n`, never inside a character.
private fun cut(text: String, n: Int): String {
    var end = n.coerceIn(0, text.length)
    if (end in 1 until text.length && Character.isHighSurrogate(text[end - 1])) end++
    return text.substring(0, end)
}

/// The first `n` characters of `blocks`, the newest of them fading in.
private fun mdHead(blocks: List<MdBlock>, n: Int): List<MdBlock> {
    val out = ArrayList<MdBlock>()
    var left = n
    for (block in blocks) {
        if (left <= 0) break
        val size = mdLength(block)
        val whole = size <= left
        out += when (block) {
            is MdBlock.Para -> block.copy(spans = spansHead(block.spans, left))
            is MdBlock.Heading -> block.copy(spans = spansHead(block.spans, left))
            is MdBlock.Item -> block.copy(spans = spansHead(block.spans, left))
            is MdBlock.Quote -> block.copy(spans = spansHead(block.spans, left))
            is MdBlock.Code -> if (whole) block else block.copy(text = cut(block.text, left))
            // A table comes in row by row.
            is MdBlock.Table -> if (whole) block else {
                var room = left - block.head.sumOf { cell -> cell.sumOf { it.text.length } }
                block.copy(rows = block.rows.takeWhile { row -> room > 0 && run { room -= row.sumOf { cell -> cell.sumOf { it.text.length } }; true } })
            }
            MdBlock.Rule -> block
        }
        left -= size
    }
    return out
}

/// `spans` cut to `n` characters, the last of them fading in: the newest
/// faintest.
private fun spansHead(spans: List<MdSpan>, n: Int): List<MdSpan> {
    val cutSpans = ArrayList<MdSpan>()
    var left = n
    for (span in spans) {
        if (left <= 0) break
        if (span.text.length <= left) cutSpans += span else cutSpans += span.copy(text = cut(span.text, left))
        left -= span.text.length
    }
    val total = cutSpans.sumOf { it.text.length }
    if (total == 0 || n >= spans.sumOf { it.text.length }) return cutSpans
    // Three bands at the end, each fainter than the one before it.
    val bands = intArrayOf(total - FADING, total - FADING * 2 / 3, total - FADING / 3, total)
    val fades = floatArrayOf(1f, 0.75f, 0.5f, 0.25f)
    val out = ArrayList<MdSpan>()
    var at = 0
    for (span in cutSpans) {
        val start = at
        val end = at + span.text.length
        at = end
        var from = start
        while (from < end) {
            val band = bands.indexOfFirst { it > from }.coerceAtLeast(0)
            var to = minOf(end, bands[band].coerceAtLeast(from + 1))
            if (to < end && Character.isHighSurrogate(span.text[to - start - 1])) to++
            out += span.copy(text = span.text.substring(from - start, to - start), fade = minOf(span.fade, fades[band]))
            from = to
        }
    }
    return out
}

/// A row that came after the chat opened comes in: it fades in as it rises
/// into its place.
@Composable
private fun Entering(fresh: Boolean, content: @Composable () -> Unit) {
    val rise = remember { Animatable(if (fresh) 1f else 0f) }
    LaunchedEffect(Unit) { if (rise.value > 0f) rise.animateTo(0f, tween(320, easing = FastOutSlowInEasing)) }
    Box(
        Modifier.graphicsLayer {
            alpha = 1f - rise.value
            translationY = rise.value * 10.dp.toPx()
        }
    ) { content() }
}

/// A row, and when it came about beside it: out of sight to its right until
/// the rows are swiped aside.
@Composable
private fun TimedRow(at: Long?, swipe: Float, gutterPx: Float, content: @Composable () -> Unit) {
    Box(Modifier.fillMaxWidth()) {
        Box(Modifier.graphicsLayer { translationX = swipe }) { content() }
        if (at != null && swipe < 0f) {
            val words = remember(at) { timeWords(at) }
            Text(
                words,
                fontSize = 11.sp,
                lineHeight = 14.sp,
                textAlign = TextAlign.End,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier
                    .align(Alignment.TopEnd)
                    .width(TIME_GUTTER - 8.dp)
                    .padding(top = 2.dp)
                    .graphicsLayer {
                        translationX = swipe + gutterPx
                        alpha = (-swipe / gutterPx).coerceIn(0f, 1f)
                    },
            )
        }
    }
}

/// When something came about, as the app's language words it: the time,
/// and the day above it when not today.
private fun timeWords(at: Long): String {
    val locale = java.util.Locale.forLanguageTag(L10n.currentTag(AppLanguage.tag))
    val then = java.time.Instant.ofEpochSecond(at).atZone(java.time.ZoneId.systemDefault())
    val today = java.time.LocalDate.now()
    val time = java.time.format.DateTimeFormatter.ofLocalizedTime(java.time.format.FormatStyle.SHORT).withLocale(locale).format(then)
    if (then.toLocalDate() == today) return time
    val skeleton = if (then.year == today.year) "MMMd" else "yMMMd"
    val day = java.time.format.DateTimeFormatter.ofPattern(android.text.format.DateFormat.getBestDateTimePattern(locale, skeleton), locale).format(then)
    return "$day\n$time"
}

/// Whether the system asks for less motion (animations turned off in its
/// accessibility or developer settings).
@Composable
internal fun rememberReducedMotion(): Boolean {
    val context = LocalContext.current
    return remember { android.provider.Settings.Global.getFloat(context.contentResolver, android.provider.Settings.Global.ANIMATOR_DURATION_SCALE, 1f) == 0f }
}

/// A small copy button: its icon turns to a check for a moment once it has
/// copied.
@Composable
private fun CopyButton(tint: Color, size: Dp, onCopy: () -> Unit) {
    var done by remember { mutableStateOf(false) }
    LaunchedEffect(done) {
        if (done) {
            delay(1500)
            done = false
        }
    }
    val view = LocalView.current
    val label = tr(if (done) "copied" else "copy")
    Box(
        Modifier
            .size(size + 22.dp)
            .clip(CircleShape)
            .clickable(role = Role.Button) {
                view.performHapticFeedback(HapticFeedbackConstants.CLOCK_TICK)
                onCopy()
                done = true
            }
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) {
        Crossfade(done, animationSpec = tween(160), label = "copied") { copied ->
            DesktopIcon(if (copied) "check" else "copy", size = size, tint = if (copied) Added else tint)
        }
    }
}

@Composable
private fun Busy(text: String, since: Long?, secondary: Color) {
    val now by produceState(System.currentTimeMillis() / 1000) {
        while (true) {
            delay(1000)
            value = System.currentTimeMillis() / 1000
        }
    }
    // Still, when the system asks for less motion.
    val still = rememberReducedMotion()
    val pulse = rememberInfiniteTransition(label = "busy")
    val phase by pulse.animateFloat(0f, 3f, infiniteRepeatable(tween(1200), RepeatMode.Restart), label = "dots")
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(9.dp)) {
        Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
            for (i in 0 until 3) {
                val lit = !still && (phase.toInt() % 3) == i
                Box(Modifier.size(6.dp).clip(CircleShape).background(secondary.copy(alpha = if (lit || (still && i == 0)) 0.9f else 0.35f)))
            }
        }
        val secs = since?.let { (now - it).coerceAtLeast(0) }
        val said = if (secs != null) "$text · ${tr("chat.secs", secs)}" else text
        // A light passes over the words, as long as it works.
        val sweep by pulse.animateFloat(-0.4f, 1.4f, infiniteRepeatable(tween(1800, easing = LinearEasing), RepeatMode.Restart), label = "sweep")
        var wide by remember { mutableIntStateOf(1) }
        val lit = LocalChrome.current.ink
        val at = sweep * wide
        val brush = Brush.horizontalGradient(
            listOf(secondary, lit.copy(alpha = 0.95f), secondary),
            startX = at - wide * 0.25f,
            endX = at + wide * 0.25f,
        )
        Text(
            said,
            style = if (still) TextStyle(color = secondary, fontSize = 13.5.sp) else TextStyle(brush = brush, fontSize = 13.5.sp),
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.onSizeChanged { wide = it.width.coerceAtLeast(1) },
        )
    }
}

/// A word from the chat said once: shown a few seconds over the field.
@Composable
private fun Notice(notice: ChatNotice?, backdrop: Backdrop, dark: Boolean) {
    var shown by remember { mutableStateOf<ChatNotice?>(null) }
    var seen by rememberSaveable { mutableStateOf(0L) }
    LaunchedEffect(notice?.seq) {
        if (notice != null && notice.seq > seen) {
            seen = notice.seq
            shown = notice
            delay(4000)
            shown = null
        }
    }
    AnimatedVisibility(shown != null, enter = fadeIn(), exit = fadeOut()) {
        Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.Center) {
            Text(
                shown?.text.orEmpty(),
                fontSize = 13.sp,
                color = LocalChrome.current.ink,
                textAlign = TextAlign.Center,
                modifier = Modifier
                    .clip(RoundedCornerShape(18.dp))
                    .glass(backdrop, dark, RoundedCornerShape(18.dp))
                    .padding(horizontal = 14.dp, vertical = 8.dp),
            )
        }
    }
}

@Composable
private fun AskCard(ask: ChatAsk, model: TerminalModel, backdrop: Backdrop, dark: Boolean, onTerminal: () -> Unit) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val shape = RoundedCornerShape(28.dp)
    Column(
        Modifier
            .fillMaxWidth()
            .glass(backdrop, dark, shape)
            .border(1.dp, Waiting.copy(alpha = 0.4f), shape)
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            DesktopIcon("triangle-alert", size = 14.dp, tint = Waiting)
            Text(ask.label, fontSize = 12.5.sp, fontWeight = FontWeight.SemiBold, color = Waiting)
        }
        Text(ask.question, fontSize = 17.sp, fontWeight = FontWeight.SemiBold, color = ink)
        if (ask.body.isNotEmpty()) {
            Box(
                Modifier
                    .fillMaxWidth()
                    .heightIn(max = 140.dp)
                    .clip(RoundedCornerShape(12.dp))
                    .background(if (dark) Color(0x5C000000) else Color(0x0F000000))
                    // A long command scrolls rather than being cut.
                    .verticalScroll(rememberScrollState())
                    .padding(horizontal = 12.dp, vertical = 10.dp),
            ) {
                SelectionContainer { Text(ask.body, fontFamily = monoFamily(), fontSize = 13.sp, lineHeight = 19.sp, color = ink) }
            }
        }
        ask.reason?.let { Text(it, fontSize = 13.sp, lineHeight = 18.sp, color = secondary) }
        ask.place?.let { Text(it, fontSize = 12.sp, color = secondary) }
        Spacer(Modifier.height(2.dp))
        // Answered only in the agent's terminal, which is under the chat.
        ask.terminal?.let { label ->
            Box(
                Modifier
                    .fillMaxWidth()
                    .height(50.dp)
                    .clip(CircleShape)
                    .background(LeadBlue)
                    .clickable(role = Role.Button, onClick = onTerminal),
                contentAlignment = Alignment.Center,
            ) {
                Text(label, fontSize = 16.5.sp, fontWeight = FontWeight.SemiBold, color = Color.White)
            }
        }
        ask.choices.firstOrNull { it.lead }?.let { lead ->
            Box(
                Modifier
                    .fillMaxWidth()
                    .height(50.dp)
                    .clip(CircleShape)
                    .background(LeadBlue)
                    .clickable { model.chatAnswer(ask.id, lead.id) },
                contentAlignment = Alignment.Center,
            ) {
                Text(lead.label, fontSize = 16.5.sp, fontWeight = FontWeight.SemiBold, color = Color.White)
            }
        }
        val others = ask.choices.filter { !it.lead }
        if (others.isNotEmpty()) {
            Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                for (choice in others) {
                    Box(
                        Modifier
                            .weight(1f)
                            .height(46.dp)
                            .clip(CircleShape)
                            .background(ink.copy(alpha = 0.12f))
                            .clickable { model.chatAnswer(ask.id, choice.id) },
                        contentAlignment = Alignment.Center,
                    ) {
                        Text(choice.label, fontSize = 15.5.sp, fontWeight = FontWeight.Medium, color = ink, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    }
                }
            }
        }
    }
}

@Composable
private fun ProblemCard(problem: ChatProblem, model: TerminalModel, backdrop: Backdrop, dark: Boolean) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val shape = RoundedCornerShape(24.dp)
    Column(
        Modifier.fillMaxWidth().glass(backdrop, dark, shape).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(problem.text, fontSize = 15.sp, lineHeight = 21.sp, color = ink)
        if (problem.detail.isNotEmpty()) {
            // Whoever wants the detail can copy it.
            SelectionContainer {
                Text(
                    problem.detail,
                    fontFamily = monoFamily(),
                    fontSize = 11.5.sp,
                    color = secondary,
                    modifier = Modifier.heightIn(max = 96.dp).verticalScroll(rememberScrollState()),
                )
            }
        }
        problem.action?.let { action ->
            Box(
                Modifier
                    .fillMaxWidth()
                    .height(46.dp)
                    .clip(CircleShape)
                    .background(LeadBlue)
                    .clickable(role = Role.Button) {
                        // The terminal is the app's to show; the rest the core's.
                        if (action.id == "terminal") model.closeChat(toTerminal = true) else model.chatAction(action.id)
                    },
                contentAlignment = Alignment.Center,
            ) {
                Text(action.label, fontSize = 15.5.sp, fontWeight = FontWeight.SemiBold, color = Color.White)
            }
        }
    }
}

/// The agent runs an older extension than the one in its folder: a line
/// over the field, and the button that has it take the new one in.
@Composable
private fun OutdatedCard(outdated: ChatOutdated, model: TerminalModel, backdrop: Backdrop, dark: Boolean) {
    val ink = LocalChrome.current.ink
    val shape = RoundedCornerShape(18.dp)
    Row(
        Modifier
            .fillMaxWidth()
            .glass(backdrop, dark, shape)
            .padding(start = 14.dp, end = 6.dp, top = 6.dp, bottom = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Text(outdated.text, fontSize = 13.sp, lineHeight = 18.sp, color = ink.copy(alpha = 0.85f), modifier = Modifier.weight(1f))
        Text(
            outdated.action.label,
            fontSize = 13.5.sp,
            fontWeight = FontWeight.SemiBold,
            color = Color.White,
            maxLines = 1,
            modifier = Modifier
                .clip(CircleShape)
                .background(LeadBlue)
                .clickable(role = Role.Button) { model.chatAction(outdated.action.id) }
                .padding(horizontal = 14.dp, vertical = 8.dp),
        )
    }
}

/// What each part of the context is drawn in.
private fun partColor(kind: String, ink: Color): Color = when (kind) {
    "system" -> Color(0xFFAF52DE)
    "tools" -> Color(0xFF5E5CE6)
    "instructions" -> Color(0xFF64D2FF)
    "user" -> Color(0xFF0A84FF)
    "assistant" -> Color(0xFF30D158)
    "thinking" -> Color(0xFFFF9F0A)
    "tool_use" -> Color(0xFFFF375F)
    "summary" -> Color(0xFFFFD60A)
    "free" -> ink.copy(alpha = 0.1f)
    else -> Color(0xFF8E8E93)
}

/// The ring's colour: the ink while there is room, amber nearly full,
/// red full.
private fun levelColor(level: String, ink: Color): Color = when (level) {
    "full" -> Removed
    "high" -> Waiting
    else -> ink.copy(alpha = 0.85f)
}

/// How full the context is, in the bar: a ring in a glass circle. A tap
/// raises what it is made of.
@Composable
private fun ContextRing(context: ChatContext, backdrop: Backdrop, dark: Boolean, onClick: () -> Unit) {
    val ink = LocalChrome.current.ink
    val fill by animateFloatAsState(context.fill ?: 0f, tween(500), label = "ring")
    val color = levelColor(context.level, ink)
    val label = "${context.heading}: ${context.percent}"
    Box(
        Modifier
            .size(40.dp)
            .glass(backdrop, dark, CircleShape)
            .clickable(role = Role.Button, onClick = onClick)
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) {
        // A whole ring under the part filled, and how full in its middle:
        // a gauge, not a spinner.
        Canvas(Modifier.size(28.dp)) {
            val stroke = 2.4.dp.toPx()
            val inset = stroke / 2
            val arc = Size(size.width - stroke, size.height - stroke)
            drawArc(ink.copy(alpha = 0.26f), 0f, 360f, false, Offset(inset, inset), arc, style = Stroke(stroke))
            if (fill > 0f) {
                drawArc(color, -90f, 360f * fill.coerceAtLeast(0.02f), false, Offset(inset, inset), arc, style = Stroke(stroke, cap = StrokeCap.Round))
            }
        }
        Text(
            context.percent.filter { it.isDigit() }.ifEmpty { "–" },
            fontSize = 9.sp,
            lineHeight = 9.sp,
            fontWeight = FontWeight.SemiBold,
            color = ink.copy(alpha = 0.85f),
            maxLines = 1,
        )
    }
}

/// How full the context is, risen from the bottom: a ring of its parts with
/// how full in its middle, when it compacts by itself, what the session has
/// used, and a way to compact now. Each part with how much opens on a tap
/// (of the ring, or the row that says so).
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ContextSheet(context: ChatContext, dark: Boolean, onDismiss: () -> Unit, onCompact: () -> Unit) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val state = rememberModalBottomSheetState()
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = state,
        containerColor = if (dark) Color(0xFF1C1C1E) else Color(0xFFF2F2F7),
        shape = RoundedCornerShape(topStart = 28.dp, topEnd = 28.dp),
    ) {
        Column(
            Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(start = 20.dp, end = 20.dp, bottom = 28.dp),
            verticalArrangement = Arrangement.spacedBy(18.dp),
        ) {
            // The parts as one ring, each its share, how full in its middle.
            var grown by remember { mutableStateOf(false) }
            var open by remember { mutableStateOf(false) }
            LaunchedEffect(Unit) { grown = true }
            val sweep by animateFloatAsState(if (grown) 1f else 0f, tween(700), label = "parts")
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(18.dp)) {
                Box(
                    Modifier
                        .size(112.dp)
                        .clip(CircleShape)
                        .clickable(enabled = context.parts.isNotEmpty(), role = Role.Button) { open = !open },
                    contentAlignment = Alignment.Center,
                ) {
                    Canvas(Modifier.fillMaxSize()) {
                        val stroke = 14.dp.toPx()
                        val inset = stroke / 2
                        val arc = Size(size.width - stroke, size.height - stroke)
                        drawArc(ink.copy(alpha = 0.08f), 0f, 360f, false, Offset(inset, inset), arc, style = Stroke(stroke))
                        var at = -90f
                        for (part in context.parts) {
                            if (part.kind == "free") continue
                            val angle = 360f * part.share * sweep
                            if (angle <= 0f) continue
                            // A hair's gap between parts.
                            drawArc(partColor(part.kind, ink), at, (angle - 1.2f).coerceAtLeast(0.6f), false, Offset(inset, inset), arc, style = Stroke(stroke, cap = StrokeCap.Butt))
                            at += angle
                        }
                    }
                    Column(horizontalAlignment = Alignment.CenterHorizontally) {
                        Text(context.percent, fontSize = 24.sp, lineHeight = 28.sp, fontWeight = FontWeight.SemiBold, color = levelColor(context.level, ink))
                    }
                }
                Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
                    Text(context.heading, fontSize = 20.sp, lineHeight = 25.sp, fontWeight = FontWeight.SemiBold, color = ink)
                    Text(context.amount, fontSize = 14.sp, lineHeight = 19.sp, color = secondary)
                    context.compactsAt?.let { Text(it, fontSize = 13.sp, lineHeight = 18.sp, color = secondary) }
                }
            }
            if (context.parts.isNotEmpty()) {
                val turn by animateFloatAsState(if (open) 90f else 0f, tween(200), label = "chevron")
                Column(
                    Modifier
                        .fillMaxWidth()
                        .clip(RoundedCornerShape(16.dp))
                        .background(ink.copy(alpha = if (dark) 0.06f else 0.04f))
                        .animateContentSize(tween(220)),
                ) {
                    Row(
                        Modifier
                            .fillMaxWidth()
                            .heightIn(min = 48.dp)
                            .clickable(role = Role.Button) { open = !open }
                            .padding(horizontal = 16.dp),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        Text(context.breakdown, fontSize = 15.sp, color = ink, modifier = Modifier.weight(1f))
                        DesktopIcon("chevron-right", size = 15.dp, tint = secondary, modifier = Modifier.graphicsLayer { rotationZ = turn })
                    }
                    if (open) for ((at, part) in context.parts.withIndex()) {
                        Box(Modifier.padding(start = if (at > 0) 40.dp else 16.dp).fillMaxWidth().height(0.5.dp).background(ink.copy(alpha = 0.08f)))
                        Row(
                            Modifier.fillMaxWidth().heightIn(min = 44.dp).padding(horizontal = 16.dp),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(12.dp),
                        ) {
                            Box(Modifier.size(10.dp).clip(CircleShape).background(partColor(part.kind, ink)))
                            Text(part.label, fontSize = 15.sp, color = ink, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                            Text(part.amount, fontSize = 14.sp, color = secondary)
                            Text(part.percent, fontSize = 14.sp, color = ink.copy(alpha = 0.8f), textAlign = TextAlign.End, modifier = Modifier.width(44.dp))
                        }
                    }
                }
            }
            if (open) context.note?.let { Text(it, fontSize = 12.5.sp, lineHeight = 17.sp, color = secondary) }
            if (context.totals.isNotEmpty()) {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text(context.totalsHeading, fontSize = 13.sp, fontWeight = FontWeight.SemiBold, color = secondary)
                    for (pair in context.totals.chunked(2)) {
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            for ((label, value) in pair) {
                                Column(
                                    Modifier
                                        .weight(1f)
                                        .clip(RoundedCornerShape(14.dp))
                                        .background(ink.copy(alpha = if (dark) 0.06f else 0.04f))
                                        .padding(horizontal = 14.dp, vertical = 10.dp),
                                ) {
                                    Text(label, fontSize = 12.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
                                    Text(value, fontSize = 18.sp, lineHeight = 24.sp, fontWeight = FontWeight.SemiBold, color = ink)
                                }
                            }
                            if (pair.size == 1) Spacer(Modifier.weight(1f))
                        }
                    }
                }
            }
            context.compact?.let { label ->
                Box(
                    Modifier
                        .fillMaxWidth()
                        .height(50.dp)
                        .clip(RoundedCornerShape(25.dp))
                        .background(ink.copy(alpha = if (dark) 0.1f else 0.07f))
                        .clickable(role = Role.Button, onClick = onCompact),
                    contentAlignment = Alignment.Center,
                ) {
                    Text(label, fontSize = 16.sp, fontWeight = FontWeight.SemiBold, color = ink)
                }
            }
        }
    }
}

/// The field to say something in, in one card with what goes with it:
/// the + for files and pictures, the model, and send -- or stop, while
/// the agent works and nothing is typed; with something typed, both. Over
/// it, as a `/` is typed, the commands the agent takes. What is typed and
/// not sent is kept for the pane's chat, as it comes and goes.
@Composable
private fun ChatField(chat: ChatView?, target: TerminalModel.ChatTarget, model: TerminalModel, backdrop: Backdrop, dark: Boolean, onSend: () -> Unit) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val view = LocalView.current
    var field by remember(target.pane) {
        val kept = model.chatDrafts[target.pane].orEmpty()
        mutableStateOf(TextFieldValue(kept, TextRange(kept.length)))
    }
    val text = field.text
    LaunchedEffect(target.pane, text) {
        if (text.isEmpty()) model.chatDrafts.remove(target.pane) else model.chatDrafts[target.pane] = text
    }
    // What the core did not send (a command only the terminal takes) comes
    // back, to be put right or sent again as words.
    var keptSeen by remember(target.pane) { mutableStateOf(chat?.notice?.seq ?: 0L) }
    LaunchedEffect(chat?.notice?.seq) {
        val notice = chat?.notice ?: return@LaunchedEffect
        if (notice.seq <= keptSeen) return@LaunchedEffect
        keptSeen = notice.seq
        val kept = notice.kept ?: return@LaunchedEffect
        if (field.text.isBlank()) field = TextFieldValue(kept, TextRange(kept.length))
    }
    val files = model.chatFiles
    // What is picked goes when all of it is there.
    val waiting = files.any { it.path == null && it.error == null }
    val ready = files.any { it.path != null }
    val canSend = chat?.canSend == true && !waiting && (text.isNotBlank() || ready)
    val working = chat?.canStop == true
    val stop = working && text.isBlank() && files.isEmpty()
    val offer = remember(chat?.commands, text) { offerFor(chat?.commands.orEmpty(), text) }
    // What was offered stays on show while it goes.
    var lastOffer by remember { mutableStateOf<Offer?>(null) }
    if (offer != null) lastOffer = offer
    val emptied = {
        field = TextFieldValue("")
        onSend()
    }
    val pick = { command: ChatCommand, from: Int ->
        view.performHapticFeedback(HapticFeedbackConstants.CLOCK_TICK)
        if (command.args == null && from == 0) {
            // One that runs as it is goes now, as typed.
            model.chatCommand(command.name)
            emptied()
        } else {
            val next = text.substring(0, from) + command.name + " "
            field = TextFieldValue(next, TextRange(next.length))
        }
    }
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        AnimatedVisibility(
            visible = offer != null,
            enter = fadeIn(tween(180)) +
                scaleIn(tween(180), initialScale = 0.96f, transformOrigin = TransformOrigin(0.5f, 1f)) +
                slideInVertically(tween(180)) { it / 16 },
            exit = fadeOut(tween(140)) + scaleOut(tween(140), targetScale = 0.98f, transformOrigin = TransformOrigin(0.5f, 1f)),
        ) {
            lastOffer?.let { shown ->
                CommandPalette(
                    shown.commands,
                    chat?.model,
                    model,
                    backdrop,
                    dark,
                    onPick = { pick(it, shown.from) },
                    onModelPicked = { field = TextFieldValue("") },
                )
            }
        }
        val shape = RoundedCornerShape(24.dp)
        Column(
            Modifier
                .fillMaxWidth()
                .glass(backdrop, dark, shape)
                .border(0.5.dp, ink.copy(alpha = if (dark) 0.12f else 0.08f), shape)
                .padding(horizontal = 4.dp, vertical = 2.dp),
        ) {
            Appearing(target.pane, files.takeIf { it.isNotEmpty() }) { picked ->
                Box(Modifier.padding(start = 8.dp, end = 8.dp, top = 8.dp)) { PickedFiles(picked, model, dark) }
            }
            // As large as the chat's text is set to be.
            val scale = chatTextScale(model.settings.chatText)
            BasicTextField(
                value = field,
                onValueChange = { field = it },
                textStyle = TextStyle(color = ink, fontSize = (16 * scale).sp, lineHeight = (22 * scale).sp),
                cursorBrush = SolidColor(LeadBlue),
                maxLines = 6,
                modifier = Modifier.fillMaxWidth().padding(start = 12.dp, end = 12.dp, top = 12.dp, bottom = 2.dp),
                decorationBox = { inner ->
                    Box {
                        if (text.isEmpty()) {
                            Text(chat?.placeholder.orEmpty(), fontSize = (16 * scale).sp, lineHeight = (22 * scale).sp, color = secondary.copy(alpha = 0.7f), maxLines = 1, overflow = TextOverflow.Ellipsis)
                        }
                        inner()
                    }
                },
            )
            Row(
                Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                if (chat?.canAttach == true) AttachMenu(model, dark)
                // The model takes the room there is, send keeps to the end.
                Box(Modifier.weight(1f)) {
                    chat?.model?.let { ModelChip(it, model, dark) }
                }
                // Something typed while it works: stopping stays at hand.
                AnimatedVisibility(
                    visible = working && !stop,
                    enter = fadeIn(tween(160)) + scaleIn(tween(160), initialScale = 0.8f),
                    exit = fadeOut(tween(120)) + scaleOut(tween(120), targetScale = 0.8f),
                ) {
                    StopButton(dark) {
                        view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                        model.chatInterrupt()
                    }
                }
                SendButton(stop, canSend, dark) {
                    if (stop) {
                        view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                        model.chatInterrupt()
                    } else {
                        view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                        model.chatSend(text.trim())
                        emptied()
                    }
                }
            }
        }
    }
}

/// Stop, beside send while something is typed: the agent can be stopped
/// without giving up what was typed.
@Composable
private fun StopButton(dark: Boolean, onClick: () -> Unit) {
    val ink = LocalChrome.current.ink
    val label = tr("chat.stop")
    Box(
        Modifier
            .minimumInteractiveComponentSize()
            .size(40.dp)
            .clip(CircleShape)
            .background(ink.copy(alpha = if (dark) 0.14f else 0.10f))
            .clickable(role = Role.Button, onClick = onClick)
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) {
        Box(Modifier.size(11.dp).clip(RoundedCornerShape(2.5.dp)).background(ink))
    }
}

/// Send, or stop: the one round button at the field's end, small, with
/// all of a finger's room around it.
@Composable
private fun SendButton(stop: Boolean, canSend: Boolean, dark: Boolean, onClick: () -> Unit) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val fill by animateColorAsState(
        when {
            stop -> ink
            canSend -> LeadBlue
            else -> ink.copy(alpha = 0.10f)
        },
        tween(160),
        label = "send",
    )
    val label = tr(if (stop) "chat.stop" else "chat.send")
    Box(
        Modifier
            .minimumInteractiveComponentSize()
            .size(40.dp)
            .clip(CircleShape)
            .background(fill)
            .clickable(enabled = stop || canSend, role = Role.Button, onClick = onClick)
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) {
        Crossfade(stop, animationSpec = tween(160), label = "send-icon") { stopping ->
            if (stopping) {
                Box(Modifier.size(11.dp).clip(RoundedCornerShape(2.5.dp)).background(listBackdrop(dark)))
            } else {
                DesktopIcon("arrow-up", size = 18.dp, tint = if (canSend) Color.White else secondary, modifier = Modifier.alpha(if (canSend) 1f else 0.55f))
            }
        }
    }
}

/// The word being typed, when it calls up commands: `/` at the start of
/// the field, which offers them all; or, anywhere, a word begun with a
/// skill's own sign (`$` for Codex, `@` for OpenCode), which offers those.
/// `from` is where the word begins: a pick takes its place.
private class Offer(val from: Int, val commands: List<ChatCommand>)

/// A command's name without its sign: what is matched against what is
/// typed (`/skill:review` and `$review` are both "review").
private fun bareName(name: String): String = name.drop(1).removePrefix("skill:").lowercase()

private fun offerFor(commands: List<ChatCommand>, text: String): Offer? {
    if (commands.isEmpty()) return null
    val from = if (text.startsWith("/")) 0 else text.indexOfLast { it.isWhitespace() } + 1
    val word = text.substring(from)
    if (word.isEmpty() || word.any { it.isWhitespace() }) return null
    val sign = word[0]
    val signs = commands.filter { it.kind == "skill" }.mapNotNull { it.name.firstOrNull() }.toSet() - '/'
    val offered = when {
        sign == '/' && from == 0 -> commands
        sign in signs -> commands.filter { it.name.startsWith(sign) }
        else -> return null
    }
    val typed = word.drop(1).lowercase()
    // Those whose names begin with it, then those whose names hold it, then
    // those whose words on what they do hold it -- each group kept whole,
    // and in its place.
    fun rank(command: ChatCommand): Int? {
        if (typed.isEmpty()) return 0
        val bare = bareName(command.name)
        return when {
            bare.startsWith(typed) || command.name.lowercase().startsWith(word.lowercase()) -> 0
            bare.contains(typed) -> 1
            command.description.lowercase().contains(typed) -> 2
            else -> null
        }
    }
    val groups = offered.map { it.group }.distinct()
    val all = offered.mapNotNull { command -> rank(command)?.let { command to it } }
        .sortedWith(compareBy({ groups.indexOf(it.first.group) }, { it.second }))
        .map { it.first }
    return if (all.isEmpty()) null else Offer(from, all)
}

/// The commands the word typed may be, over the field, by group: a tap
/// runs one that runs as it is, or puts one in the field to be followed.
@Composable
private fun CommandPalette(
    commands: List<ChatCommand>,
    menu: ChatModelMenu?,
    model: TerminalModel,
    backdrop: Backdrop,
    dark: Boolean,
    onPick: (ChatCommand) -> Unit,
    onModelPicked: () -> Unit,
) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val shape = RoundedCornerShape(22.dp)
    val list = rememberLazyListState()
    // A new word offers from its top.
    LaunchedEffect(commands.firstOrNull()?.name) { list.scrollToItem(0) }
    val headed = commands.map { it.group }.distinct().size > 1
    LazyColumn(
        state = list,
        modifier = Modifier
            .fillMaxWidth()
            .glass(backdrop, dark, shape)
            .border(0.5.dp, ink.copy(alpha = if (dark) 0.12f else 0.08f), shape)
            .heightIn(max = 300.dp),
        contentPadding = PaddingValues(vertical = 6.dp),
    ) {
        commands.forEachIndexed { at, command ->
            if (headed && command.group.isNotEmpty() && (at == 0 || commands[at - 1].group != command.group)) {
                item(key = "group:${command.group}") {
                    Text(
                        command.group,
                        fontSize = 12.sp,
                        fontWeight = FontWeight.SemiBold,
                        color = secondary,
                        modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = if (at == 0) 6.dp else 12.dp, bottom = 2.dp).semantics { heading() },
                    )
                }
            }
            item(key = command.name) {
                if (command.here == "model" && menu != null) {
                    var open by remember { mutableStateOf(false) }
                    var into by remember { mutableStateOf<String?>(null) }
                    Box {
                        CommandRow(command) { open = true }
                        DropdownMenu(expanded = open, onDismissRequest = { open = false; into = null }) {
                            ModelMenuItems(menu, model, into, onInto = { into = it }) {
                                open = false
                                into = null
                                onModelPicked()
                            }
                        }
                    }
                } else {
                    CommandRow(command) { onPick(command) }
                }
            }
        }
    }
}

@Composable
private fun CommandRow(command: ChatCommand, onClick: () -> Unit) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    Row(
        Modifier
            .fillMaxWidth()
            .heightIn(min = 48.dp)
            .clickable(role = Role.Button, onClick = onClick)
            .padding(horizontal = 12.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Box(
            Modifier.size(30.dp).clip(RoundedCornerShape(9.dp)).background(ink.copy(alpha = 0.07f)),
            contentAlignment = Alignment.Center,
        ) {
            val icon = when {
                command.here == "model" -> "cpu"
                command.kind == "skill" -> "sparkles"
                command.kind == "prompt" -> "scroll-text"
                else -> "square-slash"
            }
            DesktopIcon(icon, size = 16.dp, tint = secondary)
        }
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(1.dp)) {
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(command.name, fontSize = 14.5.sp, lineHeight = 19.sp, fontWeight = FontWeight.SemiBold, color = ink, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false))
                command.args?.takeIf { it.isNotBlank() }?.let {
                    Text(it, fontSize = 13.sp, lineHeight = 19.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
            }
            if (command.description.isNotBlank()) {
                Text(command.description, fontSize = 12.5.sp, lineHeight = 16.sp, color = secondary, maxLines = 2, overflow = TextOverflow.Ellipsis)
            }
        }
    }
}

/// The + beside the field: a photo, a file, or the clipboard's picture,
/// put on the host to go with what is said next.
@Composable
private fun AttachMenu(model: TerminalModel, dark: Boolean) {
    val context = LocalContext.current
    val ink = LocalChrome.current.ink
    var menu by remember { mutableStateOf(false) }
    val photos = rememberLauncherForActivityResult(ActivityResultContracts.PickMultipleVisualMedia(5)) { uris ->
        model.picking = false
        for (uri in uris) attachPhoto(context, model, uri)
    }
    val files = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        model.picking = false
        for (uri in uris) {
            val name = displayName(context, uri) ?: "file"
            when (context.contentResolver.getType(uri)) {
                // What agents take as pictures; a HEIF made one, as a photo is.
                "image/png", "image/jpeg", "image/gif", "image/webp" -> model.chatAttach(name, true) { copy(context, uri, it) }
                "image/heic", "image/heif" -> attachPhoto(context, model, uri)
                // Anything else (an SVG, a TIFF) goes by its path.
                else -> model.chatAttach(name, false) { copy(context, uri, it) }
            }
        }
    }
    val label = tr("chat.attach")
    Box {
        Box(
            Modifier
                .minimumInteractiveComponentSize()
                .size(40.dp)
                .clip(CircleShape)
                .background(ink.copy(alpha = if (dark) 0.10f else 0.07f))
                .clickable(role = Role.Button) { menu = true }
                .semantics { contentDescription = label },
            contentAlignment = Alignment.Center,
        ) {
            DesktopIcon("plus", size = 19.dp, tint = ink)
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            // A phone with nothing to pick with says so rather than closing.
            fun pick(what: String, launch: () -> Unit) {
                menu = false
                model.picking = true
                runCatching(launch).onFailure {
                    model.picking = false
                    model.note(tr("upload.unread", what))
                }
            }
            DropdownMenuItem(text = { Text(tr("attach.photo")) }, onClick = {
                pick(tr("attach.photo")) { photos.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly)) }
            })
            DropdownMenuItem(text = { Text(tr("attach.file")) }, onClick = {
                pick(tr("attach.file")) { files.launch(arrayOf("*/*")) }
            })
            if (clipboardHasPicture(context)) {
                DropdownMenuItem(text = { Text(tr("attach.paste")) }, onClick = {
                    menu = false
                    val clip = clipboardPicture(context)
                    if (clip != null) {
                        model.chatAttach(stamped("Pasted", extension(clip.second, "png")), true) { copy(context, clip.first, it) }
                    }
                })
            }
        }
    }
}

/// What is picked to go next, over the field: pictures small, files by
/// name and size; each on its way shows how far, and each can be taken
/// off.
@Composable
private fun PickedFiles(files: List<TerminalModel.ChatFile>, model: TerminalModel, dark: Boolean) {
    val ink = LocalChrome.current.ink
    val remove = tr("chat.attach.remove")
    val retry = tr("chat.attach.retry")
    Row(
        Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()).padding(top = 6.dp),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        for (file in files) key(file.id) {
            Box(Modifier.padding(end = 6.dp)) {
                val shape = RoundedCornerShape(12.dp)
                if (file.image && file.preview != null) {
                    Image(
                        file.preview,
                        contentDescription = file.name,
                        contentScale = ContentScale.Crop,
                        modifier = Modifier.size(64.dp).clip(shape),
                    )
                } else {
                    FileCard(file.name, sizeWords(file.size), file.image, dark, Modifier.height(64.dp))
                }
                when {
                    // Why it did not go, and a tap to send it again.
                    file.error != null -> Box(
                        Modifier
                            .matchParentSize()
                            .clip(shape)
                            .background(Color(0x99FF453A))
                            .clickable(role = Role.Button, onClickLabel = retry) { model.chatRetryFile(file) }
                            .semantics { contentDescription = "${file.name}: ${file.error}" },
                        contentAlignment = Alignment.Center,
                    ) { DesktopIcon("rotate-cw", size = 18.dp, tint = Color.White) }
                    file.path == null -> Box(
                        Modifier.matchParentSize().clip(shape).background(Color(0x59000000)),
                        contentAlignment = Alignment.Center,
                    ) {
                        val fraction = if (file.total > 0uL) file.sent.toFloat() / file.total.toFloat() else 0f
                        CircularProgressIndicator(
                            progress = { fraction },
                            modifier = Modifier.size(22.dp),
                            strokeWidth = 2.dp,
                            color = Color.White,
                            trackColor = Color.White.copy(alpha = 0.3f),
                        )
                    }
                }
                Box(
                    Modifier
                        .align(Alignment.TopEnd)
                        .offset(x = 8.dp, y = (-8).dp)
                        .size(22.dp)
                        .clip(CircleShape)
                        .background(if (dark) Color(0xFF48484A) else Color(0xFFC7C7CC))
                        .clickable(role = Role.Button) { model.chatDetach(file) }
                        .semantics { contentDescription = remove },
                    contentAlignment = Alignment.Center,
                ) {
                    DesktopIcon("x", size = 12.dp, tint = ink)
                }
            }
        }
    }
}

/// A file as a card: its kind, its name, and a word under it (its size).
@Composable
private fun FileCard(name: String, under: String?, image: Boolean, dark: Boolean, modifier: Modifier = Modifier) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    Row(
        modifier
            .widthIn(min = 120.dp, max = 220.dp)
            .clip(RoundedCornerShape(12.dp))
            .background(if (dark) Color(0xFF2C2C2E) else Color(0xFFE9E9EB))
            .padding(horizontal = 12.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        DesktopIcon(if (image) "image" else "file-text", size = 20.dp, tint = secondary)
        Column {
            Text(name, fontSize = 13.5.sp, color = ink, maxLines = 1, overflow = TextOverflow.MiddleEllipsis)
            if (under != null) Text(under, fontSize = 12.sp, color = secondary, maxLines = 1)
        }
    }
}

/// "1.2 MB".
private fun sizeWords(bytes: Long): String = when {
    bytes >= 1_000_000_000 -> String.format(java.util.Locale.ROOT, "%.1f GB", bytes / 1e9)
    bytes >= 1_000_000 -> String.format(java.util.Locale.ROOT, "%.1f MB", bytes / 1e6)
    bytes >= 1_000 -> String.format(java.util.Locale.ROOT, "%.0f KB", bytes / 1e3)
    else -> "$bytes B"
}

/// What was sent with something said: its pictures, small, a tap showing
/// one big; its files as cards.
@Composable
private fun SentFiles(files: List<ChatSent>, model: TerminalModel, dark: Boolean) {
    var shown by remember { mutableStateOf<ChatSent?>(null) }
    Row(
        Modifier.horizontalScroll(rememberScrollState()),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        for (file in files) {
            val local = file.path?.let { model.chatPictures[it] }
            val thumb = file.picture?.let { model.chatThumbs[it] }
            val picture = if (file.image) rememberPicture(thumb, local, 480) else null
            if (picture != null) {
                Image(
                    picture,
                    contentDescription = file.name,
                    contentScale = ContentScale.Crop,
                    modifier = Modifier
                        .size(132.dp)
                        .clip(RoundedCornerShape(14.dp))
                        .clickable(role = Role.Image) { shown = file },
                )
            } else {
                // Where it is on the host, to be pasted wherever it is wanted.
                val path = file.path
                FileCard(
                    file.name,
                    null,
                    file.image,
                    dark,
                    if (path != null) Modifier.clip(RoundedCornerShape(12.dp)).clickable(role = Role.Button) { model.copyToClipboard(path) } else Modifier,
                )
            }
        }
    }
    shown?.let { file ->
        PictureView(file, file.picture?.let { model.chatThumbs[it] }, file.path?.let { model.chatPictures[it] }) { shown = null }
    }
}

/// A picture over everything, as large as it fits: the phone's own copy
/// when it sent it, else the host's small one. Two fingers zoom it, a
/// double tap zooms in or back; a tap puts it away. Share hands it to
/// another app (to save it, or send it on).
@Composable
private fun PictureView(file: ChatSent, thumb: ImageBitmap?, local: File?, onClose: () -> Unit) {
    val picture = rememberPicture(thumb, local, 2048)
    val context = LocalContext.current
    var scale by remember { mutableStateOf(1f) }
    var offset by remember { mutableStateOf(Offset.Zero) }
    Dialog(onDismissRequest = onClose, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Box(
            Modifier
                .fillMaxSize()
                .background(Color.Black)
                .pointerInput(Unit) {
                    detectTapGestures(
                        onTap = { onClose() },
                        onDoubleTap = { at ->
                            if (scale > 1f) {
                                scale = 1f
                                offset = Offset.Zero
                            } else {
                                scale = 2.5f
                                // The point tapped stays under the finger.
                                offset = (Offset(size.width / 2f, size.height / 2f) - at) * 1.5f
                            }
                        },
                    )
                }
                .pointerInput(Unit) {
                    detectTransformGestures { _, pan, zoom, _ ->
                        scale = (scale * zoom).coerceIn(1f, 6f)
                        offset = if (scale == 1f) Offset.Zero else offset + pan
                    }
                },
            contentAlignment = Alignment.Center,
        ) {
            if (picture != null) {
                Image(
                    picture,
                    contentDescription = file.name,
                    contentScale = ContentScale.Fit,
                    modifier = Modifier.fillMaxSize().graphicsLayer {
                        scaleX = scale
                        scaleY = scale
                        translationX = offset.x
                        translationY = offset.y
                    },
                )
            }
            val share = tr("chat.share")
            Box(
                Modifier
                    .align(Alignment.TopEnd)
                    .statusBarsPadding()
                    .padding(12.dp)
                    .size(44.dp)
                    .clip(CircleShape)
                    .background(Color.White.copy(alpha = 0.16f))
                    .clickable(role = Role.Button) { sharePicture(context, file.name, local, picture) }
                    .semantics { contentDescription = share },
                contentAlignment = Alignment.Center,
            ) {
                DesktopIcon("share-2", size = 19.dp, tint = Color.White)
            }
        }
    }
}

/// The picture to another app, by the system's share sheet: the phone's own
/// copy as it is, else the small one shown, as a JPEG. Each share is a
/// file of its own (an app given one reads that one only), named after the
/// picture but never put anywhere else by its name; those over an hour old
/// go. The files are written off the main thread.
private fun sharePicture(context: android.content.Context, name: String, local: File?, shown: ImageBitmap?) {
    val own = local?.takeIf { it.exists() }
    val bitmap = if (own == null) shown?.asAndroidBitmap() ?: return else null
    val main = androidx.core.content.ContextCompat.getMainExecutor(context)
    kotlin.concurrent.thread(name = "share-picture") {
        runCatching {
            val root = File(context.cacheDir, "shared")
            val now = System.currentTimeMillis()
            root.listFiles()?.filter { now - it.lastModified() > SHARED_KEPT_MS }?.forEach { it.deleteRecursively() }
            val folder = File(root, java.util.UUID.randomUUID().toString()).apply { mkdirs() }
            val base = File(name).name.substringBeforeLast('.').replace(Regex("[^\\w .-]"), "_").trim('.', ' ').take(60).ifEmpty { "picture" }
            val file = if (own != null) {
                val ext = own.extension.filter { it.isLetterOrDigit() }.take(5).ifEmpty { "jpg" }
                own.copyTo(File(folder, "$base.$ext"))
            } else {
                File(folder, "$base.jpg").also { out ->
                    out.outputStream().use { bitmap!!.compress(android.graphics.Bitmap.CompressFormat.JPEG, 92, it) }
                }
            }
            check(file.canonicalPath.startsWith(folder.canonicalPath + File.separator))
            val uri = androidx.core.content.FileProvider.getUriForFile(context, context.packageName + ".files", file)
            val send = android.content.Intent(android.content.Intent.ACTION_SEND).apply {
                type = context.contentResolver.getType(uri) ?: "image/*"
                putExtra(android.content.Intent.EXTRA_STREAM, uri)
                addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
            main.execute {
                runCatching {
                    context.startActivity(android.content.Intent.createChooser(send, null).addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK))
                }
            }
        }
    }
}

/// How long a shared picture's file is kept for the app it went to.
private const val SHARED_KEPT_MS = 60 * 60 * 1000L

/// A picture to show: the phone's own file when there is one (`side` its
/// longest side at most), else the host's small one.
@Composable
private fun rememberPicture(thumb: ImageBitmap?, local: File?, side: Int): ImageBitmap? {
    val own = remember(local, side) { local?.let { runCatching { pictureOf(it, side) }.getOrNull() } }
    return own ?: thumb
}

/// The picture in `file`, its longest side at most about `side` pixels.
internal fun pictureOf(file: File, side: Int): ImageBitmap? {
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    BitmapFactory.decodeFile(file.path, bounds)
    if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
    var sample = 1
    while (maxOf(bounds.outWidth, bounds.outHeight) / (sample * 2) >= side) sample *= 2
    val options = BitmapFactory.Options().apply { inSampleSize = sample }
    return BitmapFactory.decodeFile(file.path, options)?.asImageBitmap()
}

/// The model in use, in the field beside send. A tap opens on the
/// models; one that can think more or less opens, as iOS's menus nest, on
/// how hard.
@Composable
private fun ModelChip(menu: ChatModelMenu, model: TerminalModel, dark: Boolean, modifier: Modifier = Modifier) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    var open by remember { mutableStateOf(false) }
    // The model whose efforts are on show; none, the models.
    var into by remember { mutableStateOf<String?>(null) }
    val label = tr("chat.model")
    Box(modifier) {
        Row(
            Modifier
                .minimumInteractiveComponentSize()
                .height(34.dp)
                .widthIn(max = 220.dp)
                .clip(CircleShape)
                .clickable(role = Role.Button) { open = true }
                .semantics { contentDescription = "$label: ${menu.label}" }
                .padding(horizontal = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            Text(menu.label, fontSize = 13.5.sp, color = ink.copy(alpha = 0.8f), maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false))
            DesktopIcon("chevron-down", size = 13.dp, tint = secondary)
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false; into = null }) {
            ModelMenuItems(menu, model, into, onInto = { into = it }) {
                open = false
                into = null
            }
        }
    }
}

/// The models, or one's efforts: the menu the model chip and `/model`
/// open. `onDone` once one is picked.
@Composable
private fun ModelMenuItems(menu: ChatModelMenu, model: TerminalModel, into: String?, onInto: (String?) -> Unit, onDone: () -> Unit) {
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val view = LocalView.current
    val opened = menu.models.firstOrNull { it.id == into }
    val tick: @Composable (Boolean) -> Unit = { on ->
        Box(Modifier.size(16.dp), contentAlignment = Alignment.Center) {
            if (on) DesktopIcon("check", size = 15.dp, tint = LeadBlue)
        }
    }
    if (opened == null) {
        MenuHeading(menu.modelsHeading, secondary)
        for (choice in menu.models) {
            DropdownMenuItem(
                text = { Text(choice.label, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                leadingIcon = { tick(choice.current) },
                trailingIcon = { if (choice.efforts.isNotEmpty()) DesktopIcon("chevron-right", size = 14.dp, tint = secondary) },
                onClick = {
                    if (choice.efforts.isNotEmpty()) {
                        onInto(choice.id)
                    } else {
                        view.performHapticFeedback(HapticFeedbackConstants.CLOCK_TICK)
                        onDone()
                        if (!choice.current) model.chatSetModel(choice.id, null)
                    }
                },
            )
        }
    } else {
        DropdownMenuItem(
            text = { Text(opened.label, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis) },
            leadingIcon = { DesktopIcon("chevron-left", size = 15.dp, tint = secondary) },
            onClick = { onInto(null) },
        )
        HorizontalDivider()
        MenuHeading(menu.effortsHeading, secondary)
        for (effort in opened.efforts) {
            DropdownMenuItem(
                text = { Text(effort.label) },
                leadingIcon = { tick(effort.current) },
                onClick = {
                    view.performHapticFeedback(HapticFeedbackConstants.CLOCK_TICK)
                    onDone()
                    if (!effort.current) model.chatSetModel(opened.id, effort.id)
                },
            )
        }
    }
}

@Composable
private fun MenuHeading(text: String, color: Color) {
    Text(text, fontSize = 12.sp, color = color, modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 4.dp))
}
