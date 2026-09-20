package com.roversx.thinkterm

import android.view.HapticFeedbackConstants
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyHorizontalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.ContentPaste
import androidx.compose.material.icons.filled.DesktopWindows
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Tune
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/// One key, on the bar or in the panel: what it says and what it sends.
/// The same shape as ios/Sources/KeyPanel.swift's `KeyCap`.
sealed interface KeyCap {
    val label: String

    /// A key by its DOM name, as the core takes it.
    data class Dom(
        override val label: String,
        val name: String,
        val ctrl: Boolean = false,
        val shift: Boolean = false,
    ) : KeyCap

    /// Typed text, the character itself.
    data class Txt(override val label: String) : KeyCap

    /// Ctrl or Alt held for the next key only.
    data class Sticky(override val label: String, val mod: Mod) : KeyCap

    /// The clipboard, typed into the pane.
    data class Paste(override val label: String) : KeyCap

    /// One of the App's chrome actions (a split, a zoom, a new tab).
    data class Chrome(override val label: String, val action: String) : KeyCap

    enum class Mod { CTRL, ALT }
}

/// The key bar's row and the panel's grids, and the one place a cap turns
/// into input.
object KeyCaps {
    private fun dom(label: String, name: String, ctrl: Boolean = false, shift: Boolean = false) =
        KeyCap.Dom(label, name, ctrl, shift)

    /// The bar under the terminal: the keys a soft keyboard has not got.
    val bar: List<KeyCap> get() = listOf(
        dom("esc", "Escape"),
        dom("tab", "Tab"),
        KeyCap.Sticky("ctrl", KeyCap.Mod.CTRL),
        KeyCap.Sticky("alt", KeyCap.Mod.ALT),
        dom("↑", "ArrowUp"),
        dom("↓", "ArrowDown"),
        dom("←", "ArrowLeft"),
        dom("→", "ArrowRight"),
        dom("home", "Home"),
        dom("end", "End"),
        dom("pgup", "PageUp"),
        dom("pgdn", "PageDown"),
        KeyCap.Txt("-"),
        KeyCap.Txt("/"),
        KeyCap.Txt("|"),
        KeyCap.Txt("~"),
        dom("^C", "c", ctrl = true),
        dom("^D", "d", ctrl = true),
        dom("^L", "l", ctrl = true),
        dom("^Z", "z", ctrl = true),
        dom("⌫", "Backspace"),
        KeyCap.Paste(tr("paste")),
    )

    val functions: List<KeyCap> = (1..12).map { dom("F$it", "F$it") }

    val navigation: List<KeyCap> = listOf(
        dom("ins", "Insert"),
        dom("del", "Delete"),
        dom("home", "Home"),
        dom("end", "End"),
        dom("pgup", "PageUp"),
        dom("pgdn", "PageDown"),
        dom("←", "ArrowLeft"),
        dom("↑", "ArrowUp"),
        dom("↓", "ArrowDown"),
        dom("→", "ArrowRight"),
        dom("⇧tab", "Tab", shift = true),
        dom("⏎", "Enter"),
    )

    val symbols: List<KeyCap> = listOf(
        "-", "=", "/", "|", "~", "^",
        ":", ";", "!", "*", "$", "%",
        "<", ">", "(", ")", "{", "}",
        "[", "]", "'", "\"", "`", "\\",
    ).map { KeyCap.Txt(it) }

    /// The chords a terminal wants and a phone cannot type.
    val chords: List<KeyCap> = listOf("a", "c", "d", "e", "k", "l", "r", "u", "w", "x", "y", "z")
        .map { dom("^${it.uppercase()}", it, ctrl = true) }

    /// Send one cap. Sticky Ctrl and Alt toggle here and are spent by the
    /// model on the next key; typed text joins the history.
    fun send(cap: KeyCap, model: TerminalModel, history: KeyHistory, tap: () -> Unit) {
        when (cap) {
            is KeyCap.Dom -> model.key(cap.name, ctrl = cap.ctrl, shift = cap.shift)
            is KeyCap.Txt -> {
                model.text(cap.label)
                history.record(cap.label)
            }
            is KeyCap.Sticky -> when (cap.mod) {
                KeyCap.Mod.CTRL -> model.ctrlSticky = !model.ctrlSticky
                KeyCap.Mod.ALT -> model.altSticky = !model.altSticky
            }
            is KeyCap.Paste -> model.pasteFromClipboard()
            is KeyCap.Chrome -> when (cap.action) {
                "close-tab" -> model.tabs?.current?.tab?.let { model.chromeClick("close-tab", tab = it) }
                else -> model.chromeClick(cap.action)
            }
        }
        tap()
    }
}

/// The little knock under a key, when the setting asks for it.
@Composable
fun rememberKeyTap(model: TerminalModel): () -> Unit {
    val view = LocalView.current
    return remember(view, model) {
        {
            if (model.settings.hapticKeys) {
                view.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
            }
        }
    }
}

/// The four faces of the extension panel.
enum class KeyPanelTab {
    KEYS, LAYOUT, SNIPPETS, HISTORY, COLORS;

    val title: String
        get() = when (this) {
            KEYS -> tr("p.keys")
            LAYOUT -> tr("p.layout")
            SNIPPETS -> tr("p.snippets")
            HISTORY -> tr("p.history")
            COLORS -> tr("p.colors")
        }
}

/// The panel under the key bar: the keys a phone keyboard has not got,
/// the user's snippets, what they sent last, and the colour schemes — on
/// the terminal's own background, as ios/Sources/KeyPanel.swift.
@Composable
fun KeyPanel(
    model: TerminalModel,
    tab: KeyPanelTab,
    onTab: (KeyPanelTab) -> Unit,
    onOpenSettings: () -> Unit,
) {
    val context = LocalContext.current
    val history = remember(context) { KeyHistory.get(context) }
    val store = remember(context) { SnippetStore.get(context) }
    val tap = rememberKeyTap(model)

    Column(
        Modifier
            .fillMaxWidth()
            // iOS pads for the home indicator; here the screen's own
            // safe-drawing padding already keeps the gesture bar clear.
            .height(268.dp)
            .background(model.background)
    ) {
        Row(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 10.dp)
                .padding(top = 8.dp, bottom = 6.dp),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Chip(Icons.Filled.Settings, tr("p.customize"), Modifier.weight(1f), onOpenSettings)
            Chip(Icons.Filled.ContentPaste, tr("paste"), Modifier.weight(1f)) { model.pasteFromClipboard() }
        }

        Box(Modifier.fillMaxWidth().weight(1f)) {
            when (tab) {
                KeyPanelTab.KEYS -> KeysFace(model, history, tap)
                KeyPanelTab.LAYOUT -> LayoutFace(model, history, tap)
                KeyPanelTab.SNIPPETS -> SnippetsFace(model, store, history, tap)
                KeyPanelTab.HISTORY -> HistoryFace(model, history, tap)
                KeyPanelTab.COLORS -> ColorsFace(model, tap)
            }
        }

        Box(Modifier.fillMaxWidth().height(0.5.dp).background(Color.White.copy(alpha = 0.08f)))

        SingleChoiceSegmentedButtonRow(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 10.dp, vertical = 8.dp)
        ) {
            val tabs = KeyPanelTab.entries
            tabs.forEachIndexed { index, entry ->
                SegmentedButton(
                    selected = tab == entry,
                    onClick = { onTab(entry) },
                    shape = SegmentedButtonDefaults.itemShape(index, tabs.size),
                    icon = {},
                    label = { Text(entry.title, fontSize = 12.sp, maxLines = 1) },
                )
            }
        }
    }
}

// MARK: keys

@Composable
private fun KeysFace(model: TerminalModel, history: KeyHistory, tap: () -> Unit) {
    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 10.dp, vertical = 6.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        KeyBlock(tr("k.function"), KeyCaps.functions, model, history, tap)
        KeyBlock(tr("k.navigation"), KeyCaps.navigation, model, history, tap)
        KeyBlock(tr("k.symbols"), KeyCaps.symbols, model, history, tap)
        KeyBlock(tr("k.control"), KeyCaps.chords, model, history, tap)
    }
}

/// The panes and tabs: what the desktop keeps in its menus, as keys.
@Composable
private fun LayoutFace(model: TerminalModel, history: KeyHistory, tap: () -> Unit) {
    val caps = listOf(
        KeyCap.Chrome(tr("split.right"), "split-right"),
        KeyCap.Chrome(tr("split.below"), "split-below"),
        KeyCap.Chrome(tr("zoom"), "zoom"),
        KeyCap.Chrome(tr("closepane"), "close"),
        KeyCap.Chrome(tr("newtab"), "new-tab"),
        KeyCap.Chrome(tr("closetab"), "close-tab"),
    )
    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 10.dp, vertical = 6.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        for (row in caps.chunked(2)) {
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                for (cap in row) {
                    Text(
                        cap.label,
                        color = Color.White,
                        fontSize = 14.sp,
                        maxLines = 1,
                        modifier = Modifier
                            .weight(1f)
                            .clip(RoundedCornerShape(10.dp))
                            .background(Color.White.copy(alpha = 0.07f))
                            .clickable { KeyCaps.send(cap, model, history, tap) }
                            .padding(vertical = 12.dp),
                        textAlign = androidx.compose.ui.text.style.TextAlign.Center,
                    )
                }
            }
        }
    }
}

/// One titled block of keys, six to a row. Plain rows, not a lazy grid:
/// the face above scrolls, and sixty keys are cheap to lay out.
@Composable
private fun KeyBlock(
    title: String,
    caps: List<KeyCap>,
    model: TerminalModel,
    history: KeyHistory,
    tap: () -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Text(
            title.uppercase(),
            color = Color.White.copy(alpha = 0.45f),
            fontSize = 10.sp,
            fontWeight = FontWeight.SemiBold,
            letterSpacing = 0.6.sp,
        )
        for (row in caps.chunked(6)) {
            Row(
                Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                for (cap in row) {
                    PanelKey(cap, Modifier.weight(1f)) { KeyCaps.send(cap, model, history, tap) }
                }
                // A short last row keeps the other keys their own width.
                repeat(6 - row.size) { Box(Modifier.weight(1f)) }
            }
        }
    }
}

@Composable
private fun PanelKey(cap: KeyCap, modifier: Modifier, onClick: () -> Unit) {
    Box(
        modifier
            .height(38.dp)
            .clip(RoundedCornerShape(10.dp))
            .background(Color.White.copy(alpha = 0.07f))
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            cap.label,
            color = Color.White,
            fontSize = 13.sp,
            fontFamily = FontFamily.Monospace,
            maxLines = 1,
            softWrap = false,
        )
    }
}

// MARK: snippets

@Composable
private fun SnippetsFace(
    model: TerminalModel,
    store: SnippetStore,
    history: KeyHistory,
    tap: () -> Unit,
) {
    var draft by remember { mutableStateOf<SnippetDraft?>(null) }

    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 10.dp, vertical = 6.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        for (snippet in store.items) {
            Row(
                Modifier
                    .fillMaxWidth()
                    .height(42.dp)
                    .clip(RoundedCornerShape(11.dp))
                    .background(Color.White.copy(alpha = 0.07f)),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(
                    Modifier
                        .weight(1f)
                        .fillMaxSize()
                        .clickable {
                            model.text(snippet.text)
                            if (snippet.runs) model.key("Enter")
                            history.record(snippet.text)
                            tap()
                        }
                        .padding(start = 12.dp, end = 8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    Text(
                        snippet.text,
                        color = Color.White,
                        fontSize = 13.sp,
                        fontFamily = FontFamily.Monospace,
                        maxLines = 1,
                        overflow = TextOverflow.MiddleEllipsis,
                        modifier = Modifier.weight(1f),
                    )
                    if (snippet.runs) {
                        Text(tr("p.run"), color = MaterialTheme.colorScheme.primary, fontSize = 12.sp)
                    }
                }
                Box(
                    Modifier
                        .width(40.dp)
                        .fillMaxSize()
                        .clickable { draft = SnippetDraft(snippet, isNew = false) },
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        Icons.Filled.Tune,
                        contentDescription = tr("snip.title"),
                        tint = Color.White.copy(alpha = 0.5f),
                        modifier = Modifier.size(17.dp),
                    )
                }
            }
        }

        Row(
            Modifier
                .fillMaxWidth()
                .height(42.dp)
                .clip(RoundedCornerShape(11.dp))
                .background(Color.White.copy(alpha = 0.07f))
                .clickable { draft = SnippetDraft(Snippet(text = ""), isNew = true) },
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.Center,
        ) {
            Icon(
                Icons.Filled.Add,
                contentDescription = null,
                tint = Color.White.copy(alpha = 0.65f),
                modifier = Modifier.size(18.dp),
            )
            Text(
                tr("snip.new"),
                color = Color.White.copy(alpha = 0.65f),
                fontSize = 14.sp,
                modifier = Modifier.padding(start = 6.dp),
            )
        }
    }

    draft?.let { open ->
        SnippetEditor(open, store) { draft = null }
    }
}

/// The snippet being written, new or old, for the editor's dialog.
private data class SnippetDraft(val snippet: Snippet, val isNew: Boolean)

/// One snippet's text and whether it runs itself; Delete throws it away.
@Composable
private fun SnippetEditor(draft: SnippetDraft, store: SnippetStore, onClose: () -> Unit) {
    var text by remember(draft) { mutableStateOf(draft.snippet.text) }
    var runs by remember(draft) { mutableStateOf(draft.snippet.runs) }

    AlertDialog(
        onDismissRequest = onClose,
        title = { Text(if (draft.isNew) tr("snip.new") else tr("snip.title")) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                OutlinedTextField(
                    value = text,
                    onValueChange = { text = it },
                    label = { Text(tr("snip.command")) },
                    textStyle = LocalTextStyle.current.copy(
                        fontFamily = FontFamily.Monospace,
                        fontSize = 14.sp,
                    ),
                    maxLines = 6,
                    keyboardOptions = KeyboardOptions(
                        capitalization = KeyboardCapitalization.None,
                        autoCorrectEnabled = false,
                    ),
                    modifier = Modifier.fillMaxWidth(),
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(tr("snip.enter"), fontSize = 14.sp, modifier = Modifier.weight(1f))
                    Switch(checked = runs, onCheckedChange = { runs = it })
                }
            }
        },
        confirmButton = {
            TextButton(
                enabled = text.isNotBlank(),
                onClick = {
                    if (draft.isNew) store.add(text, runs)
                    else store.replace(draft.snippet.copy(text = text, runs = runs))
                    onClose()
                },
            ) { Text(tr("save")) }
        },
        dismissButton = {
            Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                if (!draft.isNew) {
                    TextButton(onClick = {
                        store.remove(draft.snippet)
                        onClose()
                    }) { Text(tr("delete"), color = MaterialTheme.colorScheme.error) }
                }
                TextButton(onClick = onClose) { Text(tr("cancel")) }
            }
        },
    )
}

// MARK: history

@Composable
private fun HistoryFace(model: TerminalModel, history: KeyHistory, tap: () -> Unit) {
    val lines = history.lines
    if (lines.isEmpty()) {
        Text(
            tr("hist.empty"),
            color = Color.White.copy(alpha = 0.45f),
            fontSize = 13.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.fillMaxWidth().padding(top = 28.dp),
        )
        return
    }
    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 10.dp, vertical = 6.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        for (line in lines) {
            Box(
                Modifier
                    .fillMaxWidth()
                    .height(42.dp)
                    .clip(RoundedCornerShape(11.dp))
                    .background(Color.White.copy(alpha = 0.07f))
                    .clickable {
                        model.text(line)
                        tap()
                    }
                    .padding(horizontal = 12.dp),
                contentAlignment = Alignment.CenterStart,
            ) {
                Text(
                    line,
                    color = Color.White,
                    fontSize = 13.sp,
                    fontFamily = FontFamily.Monospace,
                    maxLines = 1,
                    overflow = TextOverflow.MiddleEllipsis,
                )
            }
        }
    }
}

// MARK: colours

@Composable
private fun ColorsFace(model: TerminalModel, tap: () -> Unit) {
    val settings = model.settings
    val names = remember { listOf(Schemes.FOLLOW_DESKTOP) + Schemes.names }
    // A thousand schemes ship with the app: only a lazy grid may draw them.
    LazyHorizontalGrid(
        rows = GridCells.Fixed(2),
        modifier = Modifier.fillMaxWidth().height(146.dp),
        contentPadding = PaddingValues(horizontal = 10.dp, vertical = 6.dp),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        items(names, key = { it }) { name ->
            Swatch(
                name = name,
                label = if (name == Schemes.FOLLOW_DESKTOP) tr("followhost") else name,
                picked = settings.schemeName == name,
            ) {
                settings.schemeName = name
                tap()
            }
        }
    }
}

@Composable
private fun Swatch(name: String, label: String, picked: Boolean, onClick: () -> Unit) {
    val accent = MaterialTheme.colorScheme.primary
    Column(
        Modifier.width(66.dp).clickable(onClick = onClick),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(5.dp),
    ) {
        Box(
            Modifier
                .width(64.dp)
                .height(40.dp)
                .clip(RoundedCornerShape(10.dp))
                .background(Schemes.background(name) ?: Color.White.copy(alpha = 0.12f))
                .border(
                    width = if (picked) 2.dp else 1.dp,
                    color = if (picked) accent else Color.White.copy(alpha = 0.18f),
                    shape = RoundedCornerShape(10.dp),
                ),
            contentAlignment = Alignment.Center,
        ) {
            if (name == Schemes.FOLLOW_DESKTOP) {
                Icon(
                    Icons.Filled.DesktopWindows,
                    contentDescription = null,
                    tint = Color.White.copy(alpha = 0.8f),
                    modifier = Modifier.size(18.dp),
                )
            } else {
                Text(
                    "Aa",
                    color = Schemes.foreground(name) ?: Color.White,
                    fontSize = 13.sp,
                    fontFamily = FontFamily.Monospace,
                )
            }
        }
        Text(
            label,
            color = if (picked) accent else Color.White.copy(alpha = 0.6f),
            fontSize = 9.5.sp,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            textAlign = TextAlign.Center,
        )
    }
}

// MARK: chrome

@Composable
private fun Chip(
    icon: androidx.compose.ui.graphics.vector.ImageVector,
    title: String,
    modifier: Modifier,
    onClick: () -> Unit,
) {
    Row(
        modifier
            .height(36.dp)
            .clip(RoundedCornerShape(10.dp))
            .background(Color.White.copy(alpha = 0.07f))
            .clickable(onClick = onClick),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.Center,
    ) {
        Icon(
            icon,
            contentDescription = null,
            tint = Color.White.copy(alpha = 0.75f),
            modifier = Modifier.size(17.dp),
        )
        Text(
            title,
            color = Color.White.copy(alpha = 0.75f),
            fontSize = 14.sp,
            maxLines = 1,
            modifier = Modifier.padding(start = 7.dp),
        )
    }
}
