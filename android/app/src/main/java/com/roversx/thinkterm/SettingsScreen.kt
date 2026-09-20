package com.roversx.thinkterm

import android.content.pm.PackageManager
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.DesktopWindows
import androidx.compose.material.icons.filled.Remove
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import java.util.Locale
import kotlin.math.round

/// The app's preferences, in the prototype's eight sections (the iOS
/// SettingsSheet one to one). The Settings tab shows it with no terminal;
/// a terminal's menu shows it with one, and then the text size is the
/// live pane's and the log is offered.
@Composable
fun SettingsScreen(model: TerminalModel?, showLog: MutableState<Boolean>?, onDone: (() -> Unit)?) {
    val settings = AppSettings.get(LocalContext.current)
    // Reading the tag subscribes the screen: a language change relabels it.
    AppLanguage.tag
    var pushed by remember { mutableStateOf<String?>(null) }
    val back: () -> Unit = { pushed = null }
    BackHandler(enabled = pushed != null, onBack = back)

    when (pushed) {
        "language" -> OptionScreen(tr("language"), languages(), settings.language, back) { settings.language = it }
        "theme" -> ThemeScreen(settings, back)
        "font.family" -> OptionScreen(tr("font.family"), fonts(), settings.fontFamily, back) { settings.fontFamily = it }
        "contrast" -> OptionScreen(tr("contrast"), contrasts(), settings.contrast, back) { settings.contrast = it }
        "cursor" -> OptionScreen(tr("cursor"), cursors(), settings.cursorStyle, back) { settings.cursorStyle = it }
        "resize.mode" -> OptionScreen(tr("resize.mode"), resizes(), settings.resizeMode, back) { settings.resizeMode = it }
        "k.keybar" -> KeyCapsScreen(back)
        else -> SettingsList(settings, model, showLog, onDone) { pushed = it }
    }
}

@Composable
private fun SettingsList(
    settings: AppSettings,
    model: TerminalModel?,
    showLog: MutableState<Boolean>?,
    onDone: (() -> Unit)?,
    onPush: (String) -> Unit,
) {
    Screen(tr("settings"), onBack = null, onDone = onDone) { padding ->
        Column(
            Modifier
                .padding(padding)
                .verticalScroll(rememberScrollState())
        ) {
            Section(tr("sec.general"))
            PushRow(tr("language"), label(languages(), settings.language)) { onPush("language") }

            Section(tr("sec.appearance"))
            val scheme = settings.schemeName
            PushRow(
                tr("theme"),
                if (scheme == Schemes.FOLLOW_DESKTOP) tr("followhost") else scheme,
                swatch = Schemes.background(scheme),
            ) { onPush("theme") }
            if (model != null) LiveTextSize(model, settings) else TextSizeStepper(settings)
            PushRow(tr("font.family"), label(fonts(), settings.fontFamily)) { onPush("font.family") }
            PushRow(tr("contrast"), label(contrasts(), settings.contrast)) { onPush("contrast") }

            Section(tr("sec.terminal"))
            SegmentRow(
                tr("scroll.mode"),
                listOf(tr("scroll.stepped"), tr("scroll.smooth")),
                if (settings.smoothScroll) 1 else 0,
            ) { settings.smoothScroll = it == 1 }
            SwitchRow(tr("scrollbar"), settings.scrollbar) { settings.scrollbar = it }
            PushRow(tr("cursor"), label(cursors(), settings.cursorStyle)) { onPush("cursor") }
            SwitchRow(tr("cursor.blink"), settings.cursorBlink) { settings.cursorBlink = it }
            SwitchRow(tr("bell"), settings.bell) { settings.bell = it }
            SwitchRow(tr("keyboard.incognito"), settings.incognitoKeyboard) { settings.incognitoKeyboard = it }
            PushRow(tr("resize.mode"), label(resizes(), settings.resizeMode)) { onPush("resize.mode") }

            Section(tr("sec.interface"))
            SegmentRow(
                tr("i.tabbar"),
                listOf(tr("i.onelevel"), tr("i.twolevel")),
                if (settings.tabBarLevels == "two") 1 else 0,
            ) { settings.tabBarLevels = if (it == 1) "two" else "one" }
            SwitchRow(tr("i.panebars"), settings.paneBars) { settings.paneBars = it }

            Section(tr("sec.keyboard"))
            PushRow(tr("k.keybar"), keyCaps.size.toString()) { onPush("k.keybar") }
            SwitchRow(tr("k.haptics"), settings.hapticKeys) { settings.hapticKeys = it }
            SwitchRow(tr("k.autopanel"), settings.autoKeyPanel) { settings.autoKeyPanel = it }

            Section(tr("sec.gestures"))
            SwitchRow(tr("g.pinch"), settings.pinchZoom) { settings.pinchZoom = it }
            SwitchRow(tr("g.twofinger"), settings.twoFingerHidesKeyboard) { settings.twoFingerHidesKeyboard = it }
            SwitchRow(tr("g.longpress"), settings.longPressSelects) { settings.longPressSelects = it }

            Section(tr("sec.connection"))
            SwitchRow(tr("c.autoreconnect"), settings.autoReconnect) { settings.autoReconnect = it }
            val keepAlive = settings.keepAliveSeconds
            StepperRow(
                tr("c.keepalive"),
                if (keepAlive == 0) tr("off") else "$keepAlive s",
                onMinus = { settings.keepAliveSeconds = (settings.keepAliveSeconds - 15).coerceIn(0, 300) },
                onPlus = { settings.keepAliveSeconds = (settings.keepAliveSeconds + 15).coerceIn(0, 300) },
            )
            SwitchRow(tr("c.background"), settings.keepSessionInBackground) { settings.keepSessionInBackground = it }

            if (showLog != null) {
                Section(tr("diagnostics"))
                SwitchRow(tr("showlog"), showLog.value) {
                    showLog.value = it
                    model?.wantsStats = it || settings.devMode
                }
                SwitchRow(tr("devmode"), settings.devMode) {
                    settings.devMode = it
                    model?.wantsStats = showLog.value || it
                }
                Footer(tr("showlog.foot"))
            }

            Section(tr("sec.about"))
            ValueRow(tr("version"), appVersion())
            Spacer(Modifier.height(24.dp))
        }
    }
}

// MARK: the pushed screens

/// The screen a push row opens: the choices, the current one ticked.
@Composable
private fun OptionScreen(
    title: String,
    options: List<Choice>,
    selected: String,
    onBack: () -> Unit,
    onPick: (String) -> Unit,
) {
    Screen(title, onBack = onBack, onDone = null) { padding ->
        Column(
            Modifier
                .padding(padding)
                .verticalScroll(rememberScrollState())
        ) {
            for (option in options) {
                ListItem(
                    headlineContent = { Text(option.label) },
                    trailingContent = {
                        if (option.value == selected) {
                            Icon(Icons.Default.Check, null, tint = MaterialTheme.colorScheme.primary)
                        }
                    },
                    modifier = Modifier.clickable { onPick(option.value) },
                )
            }
        }
    }
}

/// The colour schemes by name, searchable, each with its background as a
/// swatch; the host's own scheme first.
@Composable
private fun ThemeScreen(settings: AppSettings, onBack: () -> Unit) {
    var query by remember { mutableStateOf("") }
    val names = remember(query) {
        if (query.isBlank()) Schemes.names else Schemes.names.filter { it.contains(query.trim(), ignoreCase = true) }
    }
    Screen(tr("theme"), onBack = onBack, onDone = null) { padding ->
        Column(Modifier.padding(padding)) {
            OutlinedTextField(
                value = query,
                onValueChange = { query = it },
                placeholder = { Text(tr("theme.search")) },
                leadingIcon = { Icon(Icons.Default.Search, null) },
                singleLine = true,
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 16.dp, vertical = 8.dp),
            )
            LazyColumn {
                if (query.isBlank()) {
                    item {
                        ThemeRow(
                            tr("followhost"),
                            null,
                            settings.schemeName == Schemes.FOLLOW_DESKTOP,
                        ) { settings.schemeName = Schemes.FOLLOW_DESKTOP }
                    }
                }
                items(names, key = { it }) { name ->
                    ThemeRow(name, Schemes.background(name), settings.schemeName == name) {
                        settings.schemeName = name
                    }
                }
            }
        }
    }
}

@Composable
private fun ThemeRow(label: String, color: Color?, chosen: Boolean, onPick: () -> Unit) {
    ListItem(
        leadingContent = {
            if (color != null) {
                Swatch(color, RoundedCornerShape(4.dp), 22.dp)
            } else {
                Icon(Icons.Default.DesktopWindows, null, modifier = Modifier.size(22.dp))
            }
        },
        headlineContent = { Text(label, maxLines = 1, overflow = TextOverflow.Ellipsis) },
        trailingContent = {
            if (chosen) Icon(Icons.Default.Check, null, tint = MaterialTheme.colorScheme.primary)
        },
        modifier = Modifier.clickable(onClick = onPick),
    )
}

/// What the key bar carries, in its order. Read-only for now: the bar is
/// not editable yet.
@Composable
private fun KeyCapsScreen(onBack: () -> Unit) {
    Screen(tr("k.keybar"), onBack = onBack, onDone = null) { padding ->
        Column(
            Modifier
                .padding(padding)
                .verticalScroll(rememberScrollState())
        ) {
            for (cap in keyCaps) {
                ListItem(headlineContent = { Text(cap, fontFamily = FontFamily.Monospace) })
            }
            Footer(tr("k.keybar.foot"))
        }
    }
}

// MARK: rows

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun Screen(
    title: String,
    onBack: (() -> Unit)?,
    onDone: (() -> Unit)?,
    content: @Composable (PaddingValues) -> Unit,
) {
    Scaffold(
        modifier = Modifier.fillMaxSize(),
        topBar = {
            TopAppBar(
                title = { Text(title, maxLines = 1, overflow = TextOverflow.Ellipsis) },
                navigationIcon = {
                    if (onBack != null) {
                        IconButton(onClick = onBack) {
                            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = tr("back"))
                        }
                    }
                },
                actions = {
                    if (onDone != null) TextButton(onClick = onDone) { Text(tr("done")) }
                },
            )
        },
        content = content,
    )
}

@Composable
private fun Section(title: String) {
    Text(
        title,
        style = MaterialTheme.typography.labelLarge,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 20.dp, bottom = 4.dp),
    )
}

@Composable
private fun Footer(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
    )
}

/// A row that pushes a one-of-these screen and shows the choice.
@Composable
private fun PushRow(title: String, value: String, swatch: Color? = null, onClick: () -> Unit) {
    ListItem(
        headlineContent = { Text(title) },
        trailingContent = {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (swatch != null) Swatch(swatch, CircleShape, 16.dp)
                Secondary(value, Modifier.widthIn(max = 180.dp))
                Icon(
                    Icons.AutoMirrored.Filled.KeyboardArrowRight,
                    null,
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        },
        modifier = Modifier.clickable(onClick = onClick),
    )
}

@Composable
private fun ValueRow(title: String, value: String) {
    ListItem(headlineContent = { Text(title) }, trailingContent = { Secondary(value) })
}

@Composable
private fun SwitchRow(title: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    ListItem(
        headlineContent = { Text(title) },
        trailingContent = { Switch(checked = checked, onCheckedChange = onChange) },
        modifier = Modifier.clickable { onChange(!checked) },
    )
}

@Composable
private fun StepperRow(title: String, value: String, onMinus: () -> Unit, onPlus: () -> Unit) {
    ListItem(
        headlineContent = { Text(title) },
        trailingContent = {
            Row(verticalAlignment = Alignment.CenterVertically) {
                IconButton(onClick = onMinus) { Icon(Icons.Default.Remove, contentDescription = "-") }
                Secondary(value, Modifier.widthIn(min = 60.dp), TextAlign.Center)
                IconButton(onClick = onPlus) { Icon(Icons.Default.Add, contentDescription = "+") }
            }
        },
    )
}

/// The prototype's segmented row: a quiet label with the control under it.
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun SegmentRow(title: String, options: List<String>, selected: Int, onSelect: (Int) -> Unit) {
    Column(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp, vertical = 10.dp)
    ) {
        Text(
            title,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(7.dp))
        SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
            options.forEachIndexed { index, label ->
                SegmentedButton(
                    selected = index == selected,
                    onClick = { onSelect(index) },
                    shape = SegmentedButtonDefaults.itemShape(index, options.size),
                ) { Text(label, maxLines = 1, overflow = TextOverflow.Ellipsis) }
            }
        }
    }
}

@Composable
private fun Secondary(text: String, modifier: Modifier = Modifier, align: TextAlign? = null) {
    Text(
        text,
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        textAlign = align,
        modifier = modifier,
    )
}

@Composable
private fun Swatch(color: Color, shape: Shape, size: Dp) {
    Box(
        Modifier
            .size(size)
            .clip(shape)
            .background(color)
            .border(1.dp, Color.White.copy(alpha = 0.25f), shape)
    )
}

// MARK: the text size

/// The open terminal's text size, stepped live; the size a new
/// connection starts at follows it.
@Composable
private fun LiveTextSize(model: TerminalModel, settings: AppSettings) {
    StepperRow(
        tr("textsize"),
        String.format(Locale.US, "%.1f pt", model.fontPt),
        onMinus = { stepText(model, settings, -1.0) },
        onPlus = { stepText(model, settings, 1.0) },
    )
}

private fun stepText(model: TerminalModel, settings: AppSettings, by: Double) {
    model.stepFont(by)
    settings.fontSize = round(model.fontPt * (1 + 0.1 * by)).coerceIn(6.0, 40.0)
}

@Composable
private fun TextSizeStepper(settings: AppSettings) {
    StepperRow(
        tr("textsize"),
        String.format(Locale.US, "%.0f pt", settings.fontSize),
        onMinus = { settings.fontSize = (settings.fontSize - 1).coerceIn(6.0, 40.0) },
        onPlus = { settings.fontSize = (settings.fontSize + 1).coerceIn(6.0, 40.0) },
    )
}

// MARK: the choices behind the push rows

/// One choice on a pushed picker screen.
private class Choice(val value: String, val label: String)

private fun label(options: List<Choice>, value: String) =
    options.firstOrNull { it.value == value }?.label ?: value

private fun languages(): List<Choice> {
    val names = mapOf(
        "en-US" to "English",
        "zh-CN" to "简体中文",
        "ja-JP" to "日本語",
        "fr-FR" to "Français",
        "de-DE" to "Deutsch",
    )
    val tags = L10n.tags.ifEmpty { names.keys.toList() }
    return listOf(Choice("system", tr("system"))) + tags.map { Choice(it, names[it] ?: it) }
}

// The faces in the assets; the core shapes from font files, not the
// system's fonts. Changing one reconnects, since the glyph atlas is
// built for a face at connect time.
private fun fonts(): List<Choice> = listOf("JetBrains Mono", "Fira Code").map { Choice(it, it) }

private fun contrasts(): List<Choice> = listOf(
    Choice("off", tr("contrast.off")),
    Choice("3", tr("contrast.3")),
    Choice("45", tr("contrast.45")),
    Choice("7", tr("contrast.7")),
)

private fun cursors(): List<Choice> = listOf(
    Choice("auto", tr("cursor.auto")),
    Choice("block", tr("cursor.block")),
    Choice("bar", tr("cursor.bar")),
    Choice("underline", tr("cursor.underline")),
)

private fun resizes(): List<Choice> = listOf(
    Choice("auto", tr("resize.auto")),
    Choice("live", tr("resize.live")),
    Choice("release", tr("resize.release")),
)

/// The key bar's caps, in its order; KeyBar.kt sends them.
private val keyCaps: List<String> get() = KeyCaps.bar.map { it.label }

@Composable
private fun appVersion(): String {
    val context = LocalContext.current
    return remember(context) {
        runCatching {
            val info = context.packageManager.getPackageInfo(
                context.packageName,
                PackageManager.PackageInfoFlags.of(0),
            )
            "${info.versionName} (${info.longVersionCode})"
        }.getOrDefault("0 (0)")
    }
}
