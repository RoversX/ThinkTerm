package com.roversx.thinkterm

import android.graphics.BitmapFactory
import android.util.Base64
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.animation.AnimatedVisibility
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
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
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
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import com.kyant.backdrop.Backdrop
import com.kyant.backdrop.backdrops.layerBackdrop
import com.kyant.backdrop.backdrops.rememberLayerBackdrop
import kotlinx.coroutines.delay
import java.io.File
import org.json.JSONArray
import org.json.JSONObject

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
    data class Text(override val id: String, val blocks: List<MdBlock>) : ChatRow()
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
        /// "running", "stopped", "done".
        val state: String,
        val badge: String?,
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
}

data class MdSpan(val text: String, val bold: Boolean, val italic: Boolean, val code: Boolean, val strike: Boolean, val link: String?)

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
data class ChatNotice(val seq: Long, val text: String)

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
            notice = o.optJSONObject("notice")?.let { ChatNotice(it.optLong("seq"), it.optString("text")) },
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
            "text" -> ChatRow.Text(id, blocks(r.optJSONArray("blocks")))
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
            else -> null
        }
    }

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

private val Working = Color(0xFF34C759)
private val Waiting = Color(0xFFFF9F0A)
private val Resting = Color(0xFF8E8E93)
private val Added = Color(0xFF30D158)
private val Removed = Color(0xFFFF6961)

/// The chat with the agent of a pane, over that pane's terminal: what it
/// said and did, what it asks, and a field to say something. Going back
/// is going to its terminal.
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
    var barRow by remember { mutableIntStateOf(with(density) { 42.dp.roundToPx() }) }
    val list = rememberLazyListState()
    BackHandler { onBack() }

    // Opened at the end, and kept there while the person reads the end:
    // what comes next is in view as it comes.
    val rows = chat?.rows.orEmpty()
    val last = rows.lastOrNull()
    var opened by remember(target.pane) { mutableStateOf(false) }
    // Just said something: what comes of it is shown, wherever the person was.
    var follow by remember(target.pane) { mutableStateOf(false) }
    // The row on top when older ones were asked for: it stays in its place.
    var keep by remember(target.pane) { mutableStateOf<String?>(null) }
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
        // The layout is still the one before these rows came.
        val laid = list.layoutInfo
        val shown = laid.visibleItemsInfo.lastOrNull()?.index ?: -1
        val atEnd = !opened || follow || shown < 0 || shown >= laid.totalItemsCount - 3
        opened = true
        if (atEnd && last != null) {
            follow = false
            list.scrollToItem(before + rows.size - 1 + (if (chat?.busy != null) 1 else 0))
        }
    }

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
        LazyColumn(
            state = list,
            modifier = Modifier.fillMaxSize().layerBackdrop(backdrop).background(ground),
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
            items(rows, key = { it.id }) { row -> ChatRowView(row, model, dark) }
            chat?.busy?.let { busy ->
                item(key = "tt:busy") { Busy(busy, chat.since, secondary) }
            }
        }

        // The bar: back, and whom the chat is with. Its two lines reach
        // far down it: what scrolls under them is covered, and fades in
        // only below them, where the list begins -- however tall the
        // system's text size makes them.
        Box(
            Modifier
                .fillMaxWidth()
                .progressiveBlur(backdrop, ground, top = true, solid = bars.calculateTopPadding() + 6.dp + with(density) { barRow.toDp() })
                .padding(start = 10.dp, end = 10.dp, top = 6.dp + bars.calculateTopPadding(), bottom = 18.dp),
        ) {
            Row(
                Modifier.onSizeChanged { barRow = it.height },
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Box(
                    Modifier.size(42.dp).glass(backdrop, dark, CircleShape).clickable(onClick = onBack),
                    contentAlignment = Alignment.Center,
                ) {
                    DesktopIcon("chevron-left", size = 22.dp, tint = chrome.ink)
                }
                Column(Modifier.weight(1f)) {
                    Text(target.place, fontSize = 16.sp, fontWeight = FontWeight.SemiBold, color = chrome.ink, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        val dot = when (chat?.state) {
                            "working" -> Working
                            "blocked" -> Waiting
                            else -> Resting
                        }
                        Box(Modifier.size(7.dp).clip(CircleShape).background(dot))
                        val who = chat?.agent?.takeIf { it.isNotEmpty() } ?: target.agent
                        val said = listOfNotNull(who, chat?.stateLabel?.takeIf { it.isNotEmpty() }, chat?.title?.takeIf { it.isNotEmpty() })
                        Text(said.joinToString(" · "), fontSize = 12.5.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    }
                }
                chat?.model?.let { ModelButton(it, model, backdrop, dark) }
            }
        }

        // What it asks, or what is wrong, over the field to answer in.
        Column(
            Modifier
                .align(Alignment.BottomCenter)
                .fillMaxWidth()
                .imePadding()
                .navigationBarsPadding()
                .onSizeChanged { footPx = it.height }
                .padding(start = 12.dp, end = 12.dp, top = 8.dp, bottom = 10.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            // A chat's notices are numbered from one again: the count of
            // the ones shown is the chat's own.
            key(target.pane) { Notice(chat?.notice, backdrop, dark) }
            chat?.problem?.let { ProblemCard(it, model, backdrop, dark) }
            chat?.ask?.let { AskCard(it, model, backdrop, dark, onTerminal = onBack) }
            ChatField(chat, target, model, backdrop, dark, onSend = { follow = true })
        }
    }
}

@Composable
private fun ChatRowView(row: ChatRow, model: TerminalModel, dark: Boolean) {
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
                        fontSize = 15.sp,
                        lineHeight = 22.sp,
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
        is ChatRow.Text -> SelectionContainer { MdBlocks(row.blocks, ink, dark) }
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
    Column(Modifier.fillMaxWidth()) {
        Row(
            Modifier
                .fillMaxWidth()
                .heightIn(min = 28.dp)
                .clip(RoundedCornerShape(8.dp))
                .clickable(enabled = row.canOpen) { model.chatToggle(row.id) }
                .padding(vertical = 3.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(7.dp),
        ) {
            if (row.state == "running") {
                CircularProgressIndicator(Modifier.size(12.dp), strokeWidth = 1.5.dp, color = secondary)
            } else {
                DesktopIcon(if (row.open) "chevron-down" else "chevron-right", size = 13.dp, tint = secondary)
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
        if (row.open && row.body != null) {
            Box(Modifier.padding(start = 20.dp, top = 4.dp)) { FoldBodyView(row.body, dark, secondary) }
        }
    }
}

/// Work done between two things said, as one line; open, its folds.
@Composable
private fun GroupView(row: ChatRow.Group, model: TerminalModel, dark: Boolean, secondary: Color) {
    Column(Modifier.fillMaxWidth()) {
        Row(
            Modifier
                .fillMaxWidth()
                .heightIn(min = 28.dp)
                .clip(RoundedCornerShape(8.dp))
                .clickable { model.chatToggle(row.id) }
                .padding(vertical = 3.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(7.dp),
        ) {
            if (row.state == "running") {
                CircularProgressIndicator(Modifier.size(12.dp), strokeWidth = 1.5.dp, color = secondary)
            } else {
                DesktopIcon(if (row.open) "chevron-down" else "chevron-right", size = 13.dp, tint = secondary)
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
            row.badge?.let { Text(it, fontSize = 12.5.sp, color = Removed) }
        }
        if (row.open) {
            Column(
                Modifier.padding(start = 20.dp, top = 2.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                for (fold in row.rows) key(fold.id) { FoldView(fold, model, dark, secondary) }
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
fun MdBlocks(blocks: List<MdBlock>, color: Color, dark: Boolean) {
    val mono = monoFamily()
    val codeGround = if (dark) Color(0xFF17181B) else Color(0xFFF2F2F5)
    val inlineGround = if (dark) Color(0x24FFFFFF) else Color(0x14000000)
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        for (block in blocks) {
            when (block) {
                is MdBlock.Para -> Text(spans(block.spans, mono, inlineGround), fontSize = 15.sp, lineHeight = 23.sp, color = color)
                is MdBlock.Heading -> Text(
                    spans(block.spans, mono, inlineGround),
                    fontSize = when (block.level) { 1 -> 20.sp; 2 -> 18.sp; else -> 16.sp },
                    lineHeight = 26.sp,
                    fontWeight = FontWeight.SemiBold,
                    color = color,
                )
                is MdBlock.Item -> Row(Modifier.padding(start = (block.depth * 16).dp)) {
                    Text(block.mark, fontSize = 15.sp, lineHeight = 23.sp, color = color.copy(alpha = 0.7f), modifier = Modifier.width(22.dp))
                    Text(spans(block.spans, mono, inlineGround), fontSize = 15.sp, lineHeight = 23.sp, color = color)
                }
                is MdBlock.Quote -> Row(Modifier.height(IntrinsicSize.Min)) {
                    Box(Modifier.width(3.dp).fillMaxHeight().clip(CircleShape).background(color.copy(alpha = 0.3f)))
                    Text(
                        spans(block.spans, mono, inlineGround),
                        fontSize = 15.sp,
                        lineHeight = 23.sp,
                        color = color.copy(alpha = 0.75f),
                        modifier = Modifier.padding(start = 10.dp),
                    )
                }
                is MdBlock.Code -> Box(
                    Modifier
                        .fillMaxWidth()
                        .clip(RoundedCornerShape(12.dp))
                        .background(codeGround)
                        .horizontalScroll(rememberScrollState())
                        .padding(horizontal = 12.dp, vertical = 10.dp),
                ) {
                    Text(block.text, fontFamily = mono, fontSize = 13.sp, lineHeight = 19.sp, color = color, softWrap = false)
                }
                MdBlock.Rule -> Box(Modifier.fillMaxWidth().height(1.dp).background(color.copy(alpha = 0.15f)))
            }
        }
    }
}

private fun spans(spans: List<MdSpan>, mono: FontFamily, inlineGround: Color): AnnotatedString = buildAnnotatedString {
    for (span in spans) {
        val style = SpanStyle(
            fontWeight = if (span.bold) FontWeight.SemiBold else null,
            fontStyle = if (span.italic) FontStyle.Italic else null,
            fontFamily = if (span.code) mono else null,
            background = if (span.code) inlineGround else Color.Unspecified,
            textDecoration = when {
                span.strike -> TextDecoration.LineThrough
                span.link != null -> TextDecoration.Underline
                else -> null
            },
            color = if (span.link != null) LeadBlue else Color.Unspecified,
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
@Composable
private fun Busy(text: String, since: Long?, secondary: Color) {
    val now by produceState(System.currentTimeMillis() / 1000) {
        while (true) {
            delay(1000)
            value = System.currentTimeMillis() / 1000
        }
    }
    val pulse = rememberInfiniteTransition(label = "busy")
    val phase by pulse.animateFloat(0f, 3f, infiniteRepeatable(tween(1200), RepeatMode.Restart), label = "dots")
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(9.dp)) {
        Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
            for (i in 0 until 3) {
                val lit = (phase.toInt() % 3) == i
                Box(Modifier.size(6.dp).clip(CircleShape).background(secondary.copy(alpha = if (lit) 0.9f else 0.35f)))
            }
        }
        val secs = since?.let { (now - it).coerceAtLeast(0) }
        val said = if (secs != null) "$text · ${tr("chat.secs", secs)}" else text
        Text(said, fontSize = 13.5.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
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
                    .padding(horizontal = 12.dp, vertical = 10.dp),
            ) {
                Text(ask.body, fontFamily = monoFamily(), fontSize = 13.sp, lineHeight = 19.sp, color = ink)
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
            Text(problem.detail, fontFamily = monoFamily(), fontSize = 11.5.sp, color = secondary, maxLines = 3, overflow = TextOverflow.Ellipsis)
        }
        problem.action?.let { action ->
            Box(
                Modifier
                    .fillMaxWidth()
                    .height(46.dp)
                    .clip(CircleShape)
                    .background(LeadBlue)
                    .clickable { model.chatAction(action.id) },
                contentAlignment = Alignment.Center,
            ) {
                Text(action.label, fontSize = 15.5.sp, fontWeight = FontWeight.SemiBold, color = Color.White)
            }
        }
    }
}

/// The field to say something in, and the button beside it: send, or
/// stop while the agent works and nothing is typed.
@Composable
private fun ChatField(chat: ChatView?, target: TerminalModel.ChatTarget, model: TerminalModel, backdrop: Backdrop, dark: Boolean, onSend: () -> Unit) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    var text by rememberSaveable(target.pane) { mutableStateOf("") }
    val files = model.chatFiles
    // What is picked goes when all of it is there.
    val waiting = files.any { it.path == null && it.error == null }
    val ready = files.any { it.path != null }
    val canSend = chat?.canSend == true && !waiting && (text.isNotBlank() || ready)
    val stop = chat?.canStop == true && text.isBlank() && files.isEmpty()
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        if (files.isNotEmpty()) PickedFiles(files, model, dark)
        Row(verticalAlignment = Alignment.Bottom, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            if (chat?.canAttach == true) AttachMenu(model, backdrop, dark)
            BasicTextField(
                value = text,
                onValueChange = { text = it },
                textStyle = TextStyle(color = ink, fontSize = 16.sp, lineHeight = 22.sp),
                cursorBrush = SolidColor(LeadBlue),
                maxLines = 6,
                modifier = Modifier
                    .weight(1f)
                    .heightIn(min = 46.dp)
                    .glass(backdrop, dark, RoundedCornerShape(23.dp))
                    .padding(horizontal = 18.dp, vertical = 12.dp),
                decorationBox = { inner ->
                    Box {
                        if (text.isEmpty()) {
                            Text(chat?.placeholder.orEmpty(), fontSize = 16.sp, color = secondary.copy(alpha = 0.7f), maxLines = 1, overflow = TextOverflow.Ellipsis)
                        }
                        inner()
                    }
                },
            )
            Box(
                Modifier
                    .size(46.dp)
                    .clip(CircleShape)
                    .background(
                        when {
                            stop -> ink
                            canSend -> LeadBlue
                            else -> ink.copy(alpha = 0.14f)
                        }
                    )
                    .clickable(enabled = stop || canSend) {
                        if (stop) {
                            model.chatInterrupt()
                        } else {
                            model.chatSend(text.trim())
                            text = ""
                            onSend()
                        }
                    },
                contentAlignment = Alignment.Center,
            ) {
                if (stop) {
                    Box(Modifier.size(14.dp).clip(RoundedCornerShape(3.5.dp)).background(listBackdrop(dark)))
                } else {
                    DesktopIcon("arrow-up", size = 20.dp, tint = if (canSend) Color.White else secondary, modifier = Modifier.alpha(if (canSend) 1f else 0.6f))
                }
            }
        }
    }
}

/// The + beside the field: a photo, a file, or the clipboard's picture,
/// put on the host to go with what is said next.
@Composable
private fun AttachMenu(model: TerminalModel, backdrop: Backdrop, dark: Boolean) {
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
                .size(46.dp)
                .glass(backdrop, dark, CircleShape)
                .clickable(role = Role.Button) { menu = true }
                .semantics { contentDescription = label },
            contentAlignment = Alignment.Center,
        ) {
            DesktopIcon("plus", size = 21.dp, tint = ink)
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
                    file.error != null -> Box(
                        Modifier.matchParentSize().clip(shape).background(Color(0x99FF453A)),
                        contentAlignment = Alignment.Center,
                    ) { DesktopIcon("triangle-alert", size = 18.dp, tint = Color.White) }
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
                FileCard(file.name, null, file.image, dark)
            }
        }
    }
    shown?.let { file ->
        PictureView(file, file.picture?.let { model.chatThumbs[it] }, file.path?.let { model.chatPictures[it] }) { shown = null }
    }
}

/// A picture over everything, as large as it fits: the phone's own copy
/// when it sent it, else the host's small one. A tap puts it away.
@Composable
private fun PictureView(file: ChatSent, thumb: ImageBitmap?, local: File?, onClose: () -> Unit) {
    val picture = rememberPicture(thumb, local, 2048)
    Dialog(onDismissRequest = onClose, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Box(
            Modifier.fillMaxSize().background(Color.Black).clickable(onClick = onClose),
            contentAlignment = Alignment.Center,
        ) {
            if (picture != null) {
                Image(picture, contentDescription = file.name, contentScale = ContentScale.Fit, modifier = Modifier.fillMaxSize())
            }
        }
    }
}

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

/// The model in use, by the chat's name. A tap opens on the models; one
/// that can think more or less opens, as iOS's menus nest, on how hard.
@Composable
private fun ModelButton(menu: ChatModelMenu, model: TerminalModel, backdrop: Backdrop, dark: Boolean) {
    val ink = LocalChrome.current.ink
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    var open by remember { mutableStateOf(false) }
    // The model whose efforts are on show; none, the models.
    var into by remember { mutableStateOf<String?>(null) }
    val label = tr("chat.model")
    val close = {
        open = false
        into = null
    }
    Box {
        Row(
            Modifier
                .heightIn(min = 36.dp)
                .widthIn(max = 170.dp)
                .glass(backdrop, dark, RoundedCornerShape(18.dp))
                .clickable(role = Role.Button) { open = true }
                .semantics { contentDescription = "$label: ${menu.label}" }
                .padding(horizontal = 12.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(5.dp),
        ) {
            Text(menu.label, fontSize = 13.sp, color = ink, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false))
            DesktopIcon("chevron-down", size = 12.dp, tint = secondary)
        }
        DropdownMenu(expanded = open, onDismissRequest = close) {
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
                                into = choice.id
                            } else {
                                close()
                                if (!choice.current) model.chatSetModel(choice.id, null)
                            }
                        },
                    )
                }
            } else {
                DropdownMenuItem(
                    text = { Text(opened.label, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                    leadingIcon = { DesktopIcon("chevron-left", size = 15.dp, tint = secondary) },
                    onClick = { into = null },
                )
                HorizontalDivider()
                MenuHeading(menu.effortsHeading, secondary)
                for (effort in opened.efforts) {
                    DropdownMenuItem(
                        text = { Text(effort.label) },
                        leadingIcon = { tick(effort.current) },
                        onClick = {
                            close()
                            if (!effort.current) model.chatSetModel(opened.id, effort.id)
                        },
                    )
                }
            }
        }
    }
}

@Composable
private fun MenuHeading(text: String, color: Color) {
    Text(text, fontSize = 12.sp, color = color, modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 4.dp))
}
