package com.roversx.thinkterm

import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedContent
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject

// Settings, About, Open Source Licenses: what the app ships that is not its
// own, each with its license as its authors wrote it, as the iPhone app's
// page has it. The list is licenses.json, which android/build.sh makes
// (ios/tools/gen-licenses.py) from the crates linked into the core, the
// bundle's fonts and icons, the app's Java and Kotlin libraries
// (android/libraries.json) and the repository's NOTICE.

/// One component: who it is, under what, where its source is, what of it
/// the entry covers, and its license texts by id.
private class LicenseItem(
    val section: String,
    val name: String,
    val version: String,
    val license: String,
    val url: String,
    val includes: List<String>,
    val texts: List<String>,
) {
    val title get() = if (version.isEmpty()) name else "$name $version"
    val key get() = "$section/$name/$version"
}

private class LicenseList(val items: List<LicenseItem>, val texts: Map<String, String>)

/// licenses.json, read once: a couple of megabytes of text, read off the
/// main thread. None at all in a build made without android/build.sh.
private object Licenses {
    @Volatile private var loaded: LicenseList? = null

    fun get(context: Context): LicenseList = loaded ?: synchronized(this) {
        loaded ?: read(context).also { loaded = it }
    }

    private fun read(context: Context): LicenseList {
        val json = runCatching { context.assets.open("licenses.json").bufferedReader().use { it.readText() } }
            .getOrNull() ?: return LicenseList(emptyList(), emptyMap())
        val root = JSONObject(json)
        val texts = root.getJSONObject("texts").let { all -> all.keys().asSequence().associateWith { all.getString(it) } }
        val array = root.getJSONArray("items")
        val items = (0 until array.length()).map { i ->
            val item = array.getJSONObject(i)
            fun strings(name: String) = item.optJSONArray(name)?.let { list -> (0 until list.length()).map { list.getString(it) } } ?: emptyList()
            LicenseItem(
                section = item.getString("section"),
                name = item.getString("name"),
                version = item.optString("version"),
                license = item.optString("license"),
                url = item.optString("url"),
                includes = strings("includes"),
                texts = strings("texts"),
            )
        }
        return LicenseList(items, texts)
    }
}

/// The sections in the iPhone app's order, Android's own libraries before
/// the crates.
private val SECTIONS: List<Pair<String, () -> String>> = listOf(
    "app" to { "ThinkTerm" },
    "assets" to { tr("licenses.assets") },
    "android" to { tr("licenses.android") },
    "rust" to { tr("licenses.rust") },
)

@Composable
internal fun LicensesScreen(onBack: () -> Unit) {
    val context = LocalContext.current
    val list by produceState<LicenseList?>(null) { value = withContext(Dispatchers.IO) { Licenses.get(context) } }
    // Kept over a license's page: what was searched, and where the list was.
    var query by remember { mutableStateOf("") }
    val listState = rememberLazyListState()
    var open by remember { mutableStateOf<LicenseItem?>(null) }
    BackHandler(enabled = open != null) { open = null }
    AnimatedContent(
        targetState = open,
        transitionSpec = { pushTransition(targetState != null) },
        contentKey = { it?.key ?: "" },
        label = "licenses",
    ) { shown ->
        if (shown == null) {
            LicenseListPage(list, query, { query = it }, listState, onBack) { open = it }
        } else {
            LicensePage(shown, list?.texts ?: emptyMap()) { open = null }
        }
    }
}

@Composable
private fun LicenseListPage(
    list: LicenseList?,
    query: String,
    onQuery: (String) -> Unit,
    state: LazyListState,
    onBack: () -> Unit,
    onOpen: (LicenseItem) -> Unit,
) {
    val colors = groupedColors()
    val foot = WindowInsets.navigationBars.asPaddingValues().calculateBottomPadding()
    GroupedPage(tr("licenses"), onBack = onBack, scroll = false) {
        Box(Modifier.padding(horizontal = 16.dp, vertical = 8.dp)) {
            DialogField(query, onQuery, tr("licenses.search"))
        }
        val needle = query.trim()
        val shown = list?.items.orEmpty().filter { item ->
            needle.isEmpty() || item.name.contains(needle, ignoreCase = true) ||
                item.license.contains(needle, ignoreCase = true) ||
                item.includes.any { it.contains(needle, ignoreCase = true) }
        }
        // Lazily: the crates alone are some five hundred rows.
        LazyColumn(
            Modifier.fillMaxWidth().weight(1f),
            state = state,
            contentPadding = PaddingValues(bottom = 24.dp + foot),
        ) {
            for ((key, title) in SECTIONS) {
                val rows = shown.filter { it.section == key }
                if (rows.isEmpty()) continue
                item(key = "head/$key") {
                    Text(
                        title().uppercase(),
                        color = colors.secondary,
                        fontSize = 13.sp,
                        modifier = Modifier.padding(start = 32.dp, end = 32.dp, top = 22.dp, bottom = 7.dp),
                    )
                }
                itemsIndexed(rows, key = { _, item -> item.key }) { index, item ->
                    LicenseRow(item, first = index == 0, last = index == rows.lastIndex) { onOpen(item) }
                }
            }
        }
    }
}

/// A row of a section's card: the component and its license, the card's
/// corners on its first and last rows and a hairline between two.
@Composable
private fun LicenseRow(item: LicenseItem, first: Boolean, last: Boolean, onClick: () -> Unit) {
    val colors = groupedColors()
    val top = if (first) CARD_RADIUS else 0.dp
    val bottom = if (last) CARD_RADIUS else 0.dp
    Column(Modifier.padding(horizontal = 16.dp)) {
        Row(
            Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(top, top, bottom, bottom))
                .background(colors.card)
                .clickable(onClick = onClick)
                .heightIn(min = 52.dp)
                .padding(horizontal = 16.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(Modifier.weight(1f)) {
                Text(item.title, color = colors.primary, fontSize = 16.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(item.license, color = colors.secondary, fontSize = 13.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
            RowChevron()
        }
        if (!last) Spacer(Modifier.fillMaxWidth().height(0.5.dp).background(colors.hairline))
    }
}

/// One component's licenses: who it is, where its source is, then each
/// license text it ships, as paragraphs laid out as they scroll -- the
/// Rust standard library's alone is too long to lay out at once.
@Composable
private fun LicensePage(item: LicenseItem, texts: Map<String, String>, onBack: () -> Unit) {
    val colors = groupedColors()
    val context = LocalContext.current
    val foot = WindowInsets.navigationBars.asPaddingValues().calculateBottomPadding()
    // An empty string between two texts: a wider gap there.
    val paragraphs = remember(item) {
        item.texts.mapNotNull { texts[it] }.flatMap { listOf("") + it.split(Regex("\n[ \t]*\n")) }
    }
    GroupedPage(item.name, onBack = onBack, scroll = false) {
        LazyColumn(
            Modifier.fillMaxWidth().weight(1f),
            contentPadding = PaddingValues(start = 20.dp, end = 20.dp, top = 8.dp, bottom = 24.dp + foot),
        ) {
            item(key = "head") {
                Column {
                    Text(item.title, color = colors.primary, fontSize = 17.sp, fontWeight = FontWeight.SemiBold)
                    Text(item.license, color = colors.secondary, fontSize = 13.sp)
                    if (item.url.isNotEmpty()) {
                        Text(
                            item.url,
                            color = colors.accent,
                            fontSize = 13.sp,
                            modifier = Modifier.clickable {
                                runCatching { context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(item.url))) }
                            },
                        )
                    }
                    if (item.includes.isNotEmpty()) {
                        Text(
                            tr("licenses.includes", item.includes.joinToString(", ")),
                            color = colors.secondary,
                            fontSize = 13.sp,
                            modifier = Modifier.padding(top = 8.dp),
                        )
                    }
                }
            }
            items(paragraphs) { paragraph ->
                if (paragraph.isEmpty()) {
                    Spacer(Modifier.height(14.dp))
                } else {
                    SelectionContainer {
                        Text(
                            paragraph,
                            color = colors.primary,
                            fontSize = 13.sp,
                            lineHeight = 18.sp,
                            modifier = Modifier.padding(top = 10.dp),
                        )
                    }
                }
            }
        }
    }
}
