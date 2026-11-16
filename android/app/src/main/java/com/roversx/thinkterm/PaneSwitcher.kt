package com.roversx.thinkterm

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.requiredSize
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.wrapContentSize
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.kyant.backdrop.Backdrop

/// The card heading a pane's capsule, as the desktop draws it: a lit circle
/// carrying the mark of what the pane runs, drawn by the core. A busy pane's
/// mark gives way to a spinner, which shows alone where there is no card.
@Composable
fun PaneCard(model: TerminalModel, id: String?, size: Dp, busy: Boolean, dark: Boolean) {
    val density = LocalDensity.current
    val pixels = with(density) { size.roundToPx() }
    val cards = if (id == null) null else remember(id, pixels, dark, busy, model.core.tabIconsRevision()) {
        CorePictures.cards(model.core, listOf(id), pixels, density.density, dark, glyphs = !busy)
    }
    Box(Modifier.size(size), contentAlignment = Alignment.Center) {
        if (cards != null) {
            // The picture at its own size, its card's box at this one's
            // corner: the shadows fall outside, as the desktop's do.
            Box(Modifier.matchParentSize().wrapContentSize(Alignment.TopStart, unbounded = true)) {
                Image(
                    cards.image,
                    contentDescription = null,
                    modifier = Modifier
                        .offset { IntOffset(-cards.inset, -cards.inset) }
                        .requiredSize(with(density) { cards.image.width.toDp() }, with(density) { cards.image.height.toDp() }),
                )
            }
        }
        if (busy) {
            CircularProgressIndicator(
                Modifier.size(size * if (cards != null) 0.56f else 0.7f),
                strokeWidth = 1.5.dp,
                color = if (cards != null) Color.White else ink.copy(alpha = 0.7f),
            )
        }
    }
}

/// The panes of the tab on show, in one piece of glass under the terminal:
/// a phone held upright shows one pane at a time, whole (`setOnePane`), and
/// this is where the others are. The pane on show wears its name, the rest
/// their cards alone; a tap shows another, and the `+` at the end -- or a
/// long press -- brings up the split as a map (`PaneMap`). A lone pane has
/// nothing to switch to, and no switcher. From ios/Sources/PaneSwitcher.swift.
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun PaneSwitcher(model: TerminalModel, glass: Backdrop?, onMap: () -> Unit) {
    val entries = (model.panes?.panes ?: emptyList()).flatMap { box ->
        box.members.map { it to (box.focused && it.current) }
    }
    if (entries.size <= 1) return
    val chrome = LocalChrome.current
    // In the middle, as wide as its panes; it scrolls once they outgrow
    // the screen.
    Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.Center) {
        Row(
            Modifier
                .padding(horizontal = 10.dp)
                .height(36.dp)
                .glass(glass, chrome.dark, CircleShape)
                .horizontalScroll(rememberScrollState())
                .padding(horizontal = 3.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(2.dp),
        ) {
            for ((pane, shown) in entries) {
                // Each pane by its card alone; the one on show sits on a disc.
                Box(
                    Modifier
                        .size(30.dp)
                        .clip(CircleShape)
                        .background(if (shown) chrome.ink.copy(alpha = 0.16f) else Color.Transparent)
                        .combinedClickable(onClick = { model.chromeClick("pane", pane = pane.pane) }, onLongClick = onMap),
                    contentAlignment = Alignment.Center,
                ) {
                    PaneCard(model, pane.icon ?: model.tabs?.icon, 22.dp, pane.busy, chrome.dark)
                }
            }
            Box(
                Modifier.size(30.dp).clip(CircleShape).clickable(onClick = onMap),
                contentAlignment = Alignment.Center,
            ) {
                DesktopIcon("plus", size = 16.dp, tint = chrome.ink)
            }
        }
    }
}

/// The split of the tab on show as a map over the terminal: each pane where
/// the split puts it, the one on show ringed. A tap on a pane shows it; the
/// buttons under the map split the pane on show to its right or below; a
/// long press on a pane offers to close it. It goes with the choice.
@Composable
fun PaneMap(model: TerminalModel, glass: Backdrop?, onDone: () -> Unit) {
    val chrome = LocalChrome.current
    Box(
        Modifier
            .fillMaxSize()
            // What is behind takes it away.
            .background(Color.Black.copy(alpha = 0.4f))
            .clickable(interactionSource = null, indication = null, onClick = onDone),
        contentAlignment = Alignment.Center,
    ) {
        Column(
            Modifier
                .padding(horizontal = 24.dp)
                .widthIn(max = 340.dp)
                .glass(glass, chrome.dark, RoundedCornerShape(28.dp))
                // Taps on the panel are its own.
                .clickable(interactionSource = null, indication = null) {}
                .padding(16.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            Text(tr("panes.title"), color = chrome.ink, fontWeight = FontWeight.SemiBold, fontSize = 17.sp)
            val panes = model.panes
            if (panes != null && panes.cols > 0 && panes.rows > 0) Map(model, panes, onDone)
            Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                SplitButton(tr("pane.split.right"), Modifier.weight(1f)) { model.chromeClick("split-right"); onDone() }
                SplitButton(tr("pane.split.below"), Modifier.weight(1f)) { model.chromeClick("split-below"); onDone() }
            }
        }
    }
}

/// The split in the tab's own shape (a cell is about twice as high as it is
/// wide), kept between a square and a strip: a phone's tab is a tower, and a
/// map of one would cover the terminal it is over.
@Composable
private fun Map(model: TerminalModel, panes: PanesView, onDone: () -> Unit) {
    val shape = (panes.cols.toFloat() / (panes.rows * 2f)).coerceIn(1.3f, 2.2f)
    BoxWithConstraints(Modifier.fillMaxWidth().aspectRatio(shape)) {
        val unitW = maxWidth / panes.cols
        val unitH = maxHeight / panes.rows
        for (box in panes.panes) {
            Box(
                Modifier
                    .offset(x = unitW * box.left + 3.dp, y = unitH * box.top + 3.dp)
                    .requiredSize(
                        (unitW * box.cols - 6.dp).coerceAtLeast(1.dp),
                        (unitH * box.rows - 6.dp).coerceAtLeast(1.dp),
                    ),
            ) {
                MapCell(model, box, onDone)
            }
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun MapCell(model: TerminalModel, box: PaneBox, onDone: () -> Unit) {
    val chrome = LocalChrome.current
    val front = box.members.firstOrNull { it.current } ?: box.members.firstOrNull()
    val shape = RoundedCornerShape(14.dp)
    var menu by remember(box.pane) { mutableStateOf(false) }
    Box {
        Column(
            Modifier
                .fillMaxSize()
                .clip(shape)
                .background(Color.Black.copy(alpha = 0.28f))
                .border(if (box.focused) 2.dp else 1.dp, if (box.focused) LeadBlue else chrome.ink.copy(alpha = 0.12f), shape)
                .combinedClickable(
                    onClick = { model.chromeClick("pane", pane = box.pane); onDone() },
                    onLongClick = { menu = true },
                ),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(6.dp, Alignment.CenterVertically),
        ) {
            PaneCard(model, front?.icon ?: model.tabs?.icon, 32.dp, front?.busy ?: false, chrome.dark)
            Text(
                front?.let { it.title.ifEmpty { "shell" } } ?: "",
                color = if (box.focused) chrome.ink else chrome.ink.copy(alpha = 0.6f),
                fontSize = 12.sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.padding(horizontal = 4.dp),
            )
            if (box.members.size > 1) {
                // A stack: how many more are behind the one in front.
                Text("+${box.members.size - 1}", color = chrome.ink.copy(alpha = 0.6f), fontSize = 11.sp)
            }
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(
                text = { Text(tr("pane.close")) },
                onClick = { menu = false; model.chromeClick("close-pane", pane = box.pane); onDone() },
            )
        }
    }
}

@Composable
private fun SplitButton(label: String, modifier: Modifier, onClick: () -> Unit) {
    val chrome = LocalChrome.current
    Box(
        modifier
            .height(42.dp)
            .clip(CircleShape)
            .background(chrome.ink.copy(alpha = 0.1f))
            .clickable(onClick = onClick)
            .padding(horizontal = 10.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, color = chrome.ink, fontSize = 14.sp, fontWeight = FontWeight.Medium, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}
