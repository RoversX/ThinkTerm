package com.roversx.thinkterm

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.progressSemantics
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.kyant.backdrop.backdrops.layerBackdrop
import com.kyant.backdrop.backdrops.rememberLayerBackdrop

/// The desktop's right sidebar on a phone: the left drawer's mirror, under
/// the screen at its right edge, which slides off it by the button at the
/// bar's right end and comes back with a tap on what shows of it. Two of
/// the desktop's panels to begin with: the agents and what each is doing,
/// and the snippets. From ios/Sources/RightPanel.swift.
@Composable
fun RightPanel(model: TerminalModel, done: () -> Unit) {
    val chrome = LocalChrome.current
    val ground = listBackdrop(chrome.dark)
    var agentsFace by rememberSaveable { mutableStateOf(true) }
    val backdrop = rememberLayerBackdrop()
    val bars = WindowInsets.statusBars.asPaddingValues()
    val foot = WindowInsets.navigationBars.asPaddingValues()

    Box(Modifier.fillMaxSize().background(ground)) {
        LazyColumn(
            Modifier.fillMaxSize().layerBackdrop(backdrop).background(ground),
            contentPadding = PaddingValues(
                start = 10.dp,
                end = 10.dp,
                top = bars.calculateTopPadding() + 64.dp,
                bottom = 24.dp + foot.calculateBottomPadding(),
            ),
        ) {
            item {
                if (agentsFace) Agents(model, chrome.dark, done) else SnippetRows(model, done)
            }
        }
        // Which panel: one piece of glass, the one on show lit.
        Box(
            Modifier
                .fillMaxWidth()
                .progressiveBlur(backdrop, ground, top = true)
                .padding(start = 14.dp, end = 14.dp, top = 6.dp + bars.calculateTopPadding(), bottom = 16.dp),
        ) {
            Row(
                Modifier.fillMaxWidth().glass(backdrop, chrome.dark, CircleShape).padding(4.dp),
                horizontalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                Face(tr("panel.agents"), agentsFace, Modifier.weight(1f)) { agentsFace = true }
                Face(tr("p.snippets"), !agentsFace, Modifier.weight(1f)) { agentsFace = false }
            }
        }
    }
}

@Composable
private fun Face(label: String, on: Boolean, modifier: Modifier, onClick: () -> Unit) {
    val primary = MaterialTheme.colorScheme.onSurface
    Box(
        modifier
            .height(38.dp)
            .clip(CircleShape)
            .background(if (on) primary.copy(alpha = 0.14f) else Color.Transparent)
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            label,
            fontSize = 14.sp,
            fontWeight = if (on) FontWeight.SemiBold else FontWeight.Normal,
            color = if (on) primary else MaterialTheme.colorScheme.onSurfaceVariant,
            maxLines = 1,
        )
    }
}

/// The states in the order a person wants them: what waits on them first,
/// then what went wrong, what runs, what is done, what rests.
private val ORDER = listOf("blocked", "error", "working", "done", "idle", "unknown")

@Composable
private fun Agents(model: TerminalModel, dark: Boolean, done: () -> Unit) {
    val rows = model.agents?.rows ?: emptyList()
    Column {
        if (rows.isEmpty()) {
            Text(
                model.agents?.summary ?: "",
                fontSize = 14.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.fillMaxWidth().padding(top = 40.dp),
                textAlign = androidx.compose.ui.text.style.TextAlign.Center,
            )
            return@Column
        }
        for (state in ORDER) {
            val group = rows.filter { it.state == state }
            val first = group.firstOrNull() ?: continue
            Text(
                first.stateLabel,
                fontSize = 13.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(start = 12.dp, end = 12.dp, top = 14.dp, bottom = 2.dp),
            )
            for (row in group) {
                // What it waits for and what it said about itself, side by
                // side as the desktop's and the web's rows have them; else
                // its title.
                val said = listOfNotNull(row.kind, row.said).filter { it.isNotEmpty() }.joinToString(" · ")
                val detail = said.ifEmpty { row.title }.ifEmpty { null }
                val card = row.paneCard ?: model.tabs?.icon
                val more = row.progress != null || row.subtasksSummary.isNotEmpty()
                HomeRow(
                    lead = { ThreadCards(model.core, listOfNotNull(card), 30.dp, dark) },
                    mark = { ThreadMark(status(row.state), "Open", true, 9.dp, dark) },
                    title = row.place.ifEmpty { row.name },
                    subtitle = detail,
                    // An agent a chat can be had with: its own button opens
                    // the chat, whatever a tap on the row does.
                    trailing = if (row.chat) ({
                        ChatButton(waiting = row.state == "blocked") {
                            model.openChat(row)
                            done()
                        }
                    }) else null,
                    below = if (more) ({ AgentMore(model, row, dark) }) else null,
                ) {
                    if (row.chat && model.settings.chatFirst) {
                        model.openChat(row)
                    } else {
                        model.chromeClick("pane", pane = row.pane)
                    }
                    done()
                }
            }
        }
    }
}

/// The round button that opens an agent's chat, lit while it waits on you.
@Composable
private fun ChatButton(waiting: Boolean, onClick: () -> Unit) {
    val tint = if (waiting) LeadBlue else MaterialTheme.colorScheme.onSurfaceVariant
    val label = tr("chat.open")
    Box(
        Modifier
            .size(32.dp)
            .clip(CircleShape)
            .background(tint.copy(alpha = if (waiting) 0.16f else 0.1f))
            .clickable(role = Role.Button, onClickLabel = label, onClick = onClick)
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) {
        DesktopIcon("message-circle", size = 16.dp, tint = tint)
    }
}

/// What an agent said of its progress under its row, as the web's panel
/// shows it: how far it got, and its sub-tasks folded into one line that a
/// tap opens into the list.
@Composable
private fun AgentMore(model: TerminalModel, row: AgentRow, dark: Boolean) {
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    Column(Modifier.fillMaxWidth().padding(bottom = if (row.subtasksOpen) 0.dp else 4.dp)) {
        row.progress?.let { progress ->
            val done = progress.coerceIn(0, 100) / 100f
            Box(
                Modifier
                    .padding(vertical = 6.dp)
                    .fillMaxWidth()
                    .height(3.dp)
                    .progressSemantics(done)
                    .clip(CircleShape)
                    .background(secondary.copy(alpha = 0.22f)),
            ) {
                Box(Modifier.fillMaxHeight().fillMaxWidth(done).clip(CircleShape).background(MaterialTheme.colorScheme.primary))
            }
        }
        if (row.subtasksSummary.isNotEmpty()) {
            // A target of its own inside the row: it opens or folds the list
            // rather than showing the pane.
            Row(
                Modifier
                    .fillMaxWidth()
                    .heightIn(min = 36.dp)
                    .clickable(role = Role.Button) { model.toggleSubtasks(row.pane) },
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                DesktopIcon(
                    "chevron-right",
                    size = 12.dp,
                    tint = secondary,
                    modifier = Modifier.rotate(if (row.subtasksOpen) 90f else 0f),
                )
                Text(row.subtasksSummary, fontSize = 13.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
            if (row.subtasksOpen) {
                Column(
                    Modifier.padding(start = 18.dp, bottom = 8.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    for (sub in row.subtasks) {
                        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                            ThreadMark(status(sub.state), "Open", true, 7.dp, dark)
                            Text(sub.label, fontSize = 13.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        }
                    }
                    if (row.moreSubtasks > 0) {
                        Text(tr("agents.more", row.moreSubtasks), fontSize = 13.sp, color = secondary)
                    }
                }
            }
        }
    }
}

/// An agent's state as the thread's whose mark stands for it.
private fun status(state: String) = when (state) {
    "working" -> "Running"
    "blocked" -> "NeedsAttention"
    // Stopped on an error: the desktop's red cross, not a wait's amber.
    "error" -> "Failed"
    "done" -> "Done"
    else -> "Idle"
}

@Composable
private fun SnippetRows(model: TerminalModel, done: () -> Unit) {
    val context = LocalContext.current
    val snippets = remember(context) { SnippetStore.get(context) }
    val history = remember(context) { KeyHistory.get(context) }
    val tap = rememberKeyTap(model)
    Column {
        for (snippet in snippets.items) {
            Row(
                Modifier
                    .fillMaxWidth()
                    .heightIn(min = 44.dp)
                    .clip(CircleShape)
                    .clickable {
                        model.text(snippet.text)
                        if (snippet.runs) model.key("Enter")
                        history.record(snippet.text)
                        tap()
                        done()
                    }
                    .padding(horizontal = 12.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                Text(
                    snippet.text,
                    fontSize = 14.sp,
                    fontFamily = FontFamily.Monospace,
                    color = MaterialTheme.colorScheme.onSurface,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (snippet.runs) Text(tr("p.run"), fontSize = 13.sp, color = LeadBlue)
            }
        }
    }
}
