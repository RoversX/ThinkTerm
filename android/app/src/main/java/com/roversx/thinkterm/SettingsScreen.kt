package com.roversx.thinkterm

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.BitmapFactory
import android.net.Uri
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedContent
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.Image
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
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
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import java.util.Locale
import kotlinx.coroutines.delay

/// The app's preferences, in the prototype's eight sections (the iOS
/// SettingsSheet one to one). The Settings tab shows it with no terminal;
/// a terminal's menu shows it with one, and then the text size is the
/// live pane's and the log is offered.
@Composable
fun SettingsScreen(model: TerminalModel?, showLog: MutableState<Boolean>?, onDone: (() -> Unit)?) {
    val settings = AppSettings.get(LocalContext.current)
    // Reading the tag subscribes the screen: a language change relabels it.
    AppLanguage.tag
    // The pages gone into, from Settings' list: a section's page, and a
    // choice's screen over it.
    var pushed by remember { mutableStateOf(listOf<String>()) }
    val push: (String) -> Unit = { pushed = pushed + it }
    val back: () -> Unit = { pushed = pushed.dropLast(1) }
    BackHandler(enabled = pushed.isNotEmpty(), onBack = back)

    AnimatedContent(
        targetState = pushed,
        transitionSpec = { pushTransition(targetState.size > initialState.size) },
        contentKey = { it.lastOrNull() ?: "" },
        label = "settings",
    ) { shown ->
        when (val top = shown.lastOrNull()) {
            null -> SettingsHome(showLog != null, onDone, push)
            "language" -> OptionScreen(tr("language"), languages(), settings.language, back) { settings.language = it }
            "theme" -> ThemeScreen(settings, back)
            "font.family" -> OptionScreen(tr("font.family"), fonts(), settings.fontFamily, back) { settings.fontFamily = it }
            "contrast" -> OptionScreen(tr("contrast"), contrasts(), settings.contrast, back) { settings.contrast = it }
            "cursor" -> OptionScreen(tr("cursor"), cursors(), settings.cursorStyle, back) { settings.cursorStyle = it }
            "resize.mode" -> OptionScreen(tr("resize.mode"), resizes(), settings.resizeMode, back) { settings.resizeMode = it }
            "k.keybar" -> KeyCapsScreen(back)
            "tailscale" -> TailnetSettingsScreen(onBack = back)
            "licenses" -> LicensesScreen(onBack = back)
            else -> SettingsSection(top, settings, model, showLog, onDone, back, push)
        }
    }
}

/// What Settings has a page for, as iOS's Settings lists them: its name,
/// its icon (the desktop's), and the colour of the tile it sits on -- the
/// iOS app's, from Apple's system colours.
private class SettingsPage(val key: String, val title: () -> String, val icon: String, val tint: Color)

/// The system's colours as iOS's Settings icons wear them, the same in the
/// dark.
internal object TileColors {
    val gray = Color(0xFF8E8E93)
    val blue = Color(0xFF007AFF)
    val indigo = Color(0xFF5856D6)
    val teal = Color(0xFF30B0C7)
    val orange = Color(0xFFFF9500)
    val green = Color(0xFF34C759)
    val red = Color(0xFFFF3B30)
    val purple = Color(0xFFAF52DE)
}

private val SETTINGS_PAGES = listOf(
    SettingsPage("general", { tr("sec.general") }, "settings", TileColors.gray),
    SettingsPage("appearance", { tr("sec.appearance") }, "palette", TileColors.blue),
    SettingsPage("terminal", { tr("sec.terminal") }, "square-terminal", TileColors.indigo),
    SettingsPage("interface", { tr("sec.interface") }, "panels-top-left", TileColors.teal),
    SettingsPage("agents", { tr("sec.agents") }, "sparkles", TileColors.purple),
    SettingsPage("keyboard", { tr("sec.keyboard") }, "keyboard", TileColors.gray),
    SettingsPage("gestures", { tr("sec.gestures") }, "hand", TileColors.orange),
    SettingsPage("connection", { tr("sec.connection") }, "wifi", TileColors.green),
    SettingsPage("tailscale", { "Tailscale" }, "globe", TileColors.blue),
    SettingsPage("diagnostics", { tr("diagnostics") }, "stethoscope", TileColors.red),
    SettingsPage("about", { tr("sec.about") }, "info", TileColors.gray),
)

private fun settingsPage(key: String) = SETTINGS_PAGES.first { it.key == key }

/// Settings' list: what there is to set, each with its icon on a tile of
/// its own, in iOS's groups.
@Composable
private fun SettingsHome(diagnostics: Boolean, onDone: (() -> Unit)?, push: (String) -> Unit) {
    GroupedPage(tr("settings"), onBack = null, onDone = onDone) {
        Group {
            for (key in listOf("general", "appearance", "terminal", "interface", "agents")) SettingsHomeRow(settingsPage(key), push)
        }
        Spacer(Modifier.height(18.dp))
        Group {
            for (key in listOf("keyboard", "gestures", "connection", "tailscale")) SettingsHomeRow(settingsPage(key), push)
        }
        Spacer(Modifier.height(18.dp))
        Group {
            if (diagnostics) SettingsHomeRow(settingsPage("diagnostics"), push)
            SettingsHomeRow(settingsPage("about"), push)
        }
    }
}

@Composable
private fun SettingsHomeRow(page: SettingsPage, push: (String) -> Unit) {
    GroupRow(onClick = { push(page.key) }) {
        GlassTile(page.icon, page.tint)
        Spacer(Modifier.width(4.dp))
        RowTitle(page.title())
        RowChevron()
    }
}

/// An icon in white on a tile of colour, lit as the system's Liquid Glass
/// icons are: the colour a touch lighter at the top and deeper at the
/// foot, a faint sheen over its upper half, and a light rim along the top
/// that fades round the sides. The iOS app's GlassTile.
@Composable
internal fun GlassTile(icon: String, tint: Color, size: Dp = 30.dp) {
    val shape = RoundedCornerShape(size * 0.27f)
    Box(
        Modifier
            .size(size)
            .clip(shape)
            .background(Brush.verticalGradient(listOf(lerp(tint, Color.White, 0.1f), tint, lerp(tint, Color.Black, 0.08f))))
            .background(Brush.verticalGradient(0f to Color.White.copy(alpha = 0.12f), 0.5f to Color.White.copy(alpha = 0f)))
            .border(
                maxOf(0.6.dp, size / 48),
                Brush.verticalGradient(listOf(Color.White.copy(alpha = 0.45f), Color.White.copy(alpha = 0.05f), Color.White.copy(alpha = 0.15f))),
                shape,
            ),
        contentAlignment = Alignment.Center,
    ) {
        DesktopIcon(icon, size = size * 0.56f, tint = Color.White)
    }
}

/// The project's home, as the desktop's About links it.
private const val ABOUT_REPO = "https://github.com/RoversX/thinkterm"

/// About, as the desktop's About page has it: the app and its version,
/// what it is built as and under, where it comes from, and who makes it.
@Composable
private fun ColumnScope.AboutContent(onDone: (() -> Unit)?, onLicenses: () -> Unit) {
    val context = LocalContext.current
    val colors = groupedColors()
    val info = remember(context) {
        runCatching { context.packageManager.getPackageInfo(context.packageName, PackageManager.PackageInfoFlags.of(0)) }.getOrNull()
    }
    val version = info?.versionName ?: "0"
    val build = info?.longVersionCode?.toString() ?: "0"
    val platform = "Android ${android.os.Build.VERSION.RELEASE} · ${android.os.Build.SUPPORTED_ABIS.firstOrNull() ?: ""}"
    val icon = remember(context) {
        runCatching { context.assets.open("app_icon.png").use { BitmapFactory.decodeStream(it)?.asImageBitmap() } }.getOrNull()
    }
    var copied by remember { mutableStateOf(false) }
    LaunchedEffect(copied) {
        if (copied) {
            delay(1400)
            copied = false
        }
    }
    fun open(address: String) {
        runCatching { context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(address))) }
    }

    // The head: the app's icon, its name, its version, and the way to copy
    // what a bug report needs on its first lines.
    Column(Modifier.fillMaxWidth().padding(bottom = 18.dp), horizontalAlignment = Alignment.CenterHorizontally) {
        if (icon != null) Image(icon, contentDescription = null, modifier = Modifier.size(92.dp))
        Spacer(Modifier.height(10.dp))
        Text("ThinkTerm", color = colors.primary, fontSize = 28.sp, fontWeight = FontWeight.Bold)
        Spacer(Modifier.height(3.dp))
        Text(tr("about.versionline", version), color = colors.secondary, fontSize = 15.sp)
        Spacer(Modifier.height(14.dp))
        Row(
            Modifier
                .height(38.dp)
                .glass(LocalGroundGlass.current, colors.dark, CircleShape)
                .clickable {
                    val text = listOf(
                        "ThinkTerm $version",
                        "Build: $build",
                        "OS: Android ${android.os.Build.VERSION.RELEASE} (API ${android.os.Build.VERSION.SDK_INT})",
                        "Device: ${android.os.Build.MANUFACTURER} ${android.os.Build.MODEL}",
                    ).joinToString("\n")
                    val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
                    cm?.setPrimaryClip(ClipData.newPlainText("ThinkTerm", text))
                    copied = true
                }
                .padding(horizontal = 18.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            DesktopIcon(if (copied) "check" else "copy", size = 16.dp, tint = colors.primary)
            Spacer(Modifier.width(7.dp))
            Text(tr(if (copied) "copied" else "about.copy"), color = colors.primary, fontSize = 15.sp, fontWeight = FontWeight.SemiBold)
        }
    }

    Group {
        AboutRow("package", TileColors.blue, tr("version"), version)
        // A line of its own only when it says something the version does not.
        if (build != version) AboutRow("git-branch", TileColors.indigo, tr("about.build"), build)
        AboutRow("cpu", TileColors.teal, tr("about.platform"), platform)
        // As the packages declare it; the desktop's About says the same.
        AboutRow("scale", TileColors.orange, tr("about.license"), "GPL-3.0-only")
    }
    Spacer(Modifier.height(18.dp))
    Group {
        AboutLink(tr("about.source")) { open(ABOUT_REPO) }
        AboutLink(tr("about.releasenotes")) { open("$ABOUT_REPO/releases") }
        // In the app, as the iPhone app has them: every license it ships.
        GroupRow(onClick = onLicenses) {
            RowTitle(tr("licenses"))
            RowChevron()
        }
        AboutLink(tr("about.privacy")) { open("$ABOUT_REPO/blob/main/PRIVACY.md") }
    }
    Spacer(Modifier.height(18.dp))
    Group {
        // The first run's sky and pages again, over the app.
        GroupRow(onClick = {
            onDone?.invoke()
            LaunchSky.showAgain()
        }) { RowTitle(tr("ob.show"), color = LeadBlue) }
    }
    // Who makes it, at the foot.
    Row(
        Modifier.fillMaxWidth().padding(top = 36.dp, bottom = 12.dp),
        horizontalArrangement = Arrangement.Center,
        verticalAlignment = Alignment.Bottom,
    ) {
        // In the text's own colour, as the desktop draws it; the year a step
        // quieter.
        MakerLogo(30.dp, colors.primary)
        Spacer(Modifier.width(10.dp))
        Text("© 2026", color = colors.secondary, fontSize = 12.sp)
    }
}

/// One of About's facts: its tile, its name, and what it is.
@Composable
private fun AboutRow(icon: String, tint: Color, title: String, value: String) {
    val colors = groupedColors()
    GroupRow {
        GlassTile(icon, tint)
        Spacer(Modifier.width(4.dp))
        RowTitle(title)
        Text(value, color = colors.secondary, fontSize = 15.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}

/// A page of the project's, opened in the browser.
@Composable
private fun AboutLink(title: String, onClick: () -> Unit) {
    GroupRow(onClick = onClick) {
        RowTitle(title)
        DesktopIcon("arrow-up-right", size = 16.dp, tint = groupedColors().tertiary)
    }
}

/// One of Settings' pages: the preferences it holds, as they were.
@Composable
private fun SettingsSection(
    key: String,
    settings: AppSettings,
    model: TerminalModel?,
    showLog: MutableState<Boolean>?,
    onDone: (() -> Unit)?,
    onBack: () -> Unit,
    onPush: (String) -> Unit,
) {
    GroupedPage(settingsPage(key).title(), onBack = onBack) {
        when (key) {
            "general" -> Group {
                PushRow(tr("language"), label(languages(), settings.language)) { onPush("language") }
            }
            "appearance" -> Group {
                val scheme = settings.schemeName
                PushRow(
                    tr("theme"),
                    if (scheme == Schemes.FOLLOW_DESKTOP) tr("followhost") else scheme,
                    swatch = Schemes.background(scheme),
                ) { onPush("theme") }
                if (model != null) LiveTextSize(model) else TextSizeStepper(settings)
                PushRow(tr("font.family"), label(fonts(), settings.fontFamily)) { onPush("font.family") }
                PushRow(tr("contrast"), label(contrasts(), settings.contrast)) { onPush("contrast") }
            }
            "terminal" -> Group {
                SegmentRow(
                    tr("scroll.mode"),
                    listOf(tr("scroll.stepped"), tr("scroll.smooth")),
                    if (settings.smoothScroll) 1 else 0,
                ) { settings.smoothScroll = it == 1 }
                SwitchRow(tr("scrollbar"), settings.scrollbar) { settings.scrollbar = it }
                PushRow(tr("cursor"), label(cursors(), settings.cursorStyle)) { onPush("cursor") }
                SwitchRow(tr("cursor.blink"), settings.cursorBlink) { settings.cursorBlink = it }
                SwitchRow(tr("bell"), settings.bell) { settings.bell = it }
                PushRow(tr("resize.mode"), label(resizes(), settings.resizeMode)) { onPush("resize.mode") }
            }
            "interface" -> Group {
                SwitchRow(tr("i.panebars"), settings.paneBars) { settings.paneBars = it }
                SegmentRow(
                    tr("i.spaces"),
                    listOf(tr("i.spaces.list"), tr("i.spaces.menu")),
                    if (settings.spaces == "menu") 1 else 0,
                ) { settings.spaces = if (it == 1) "menu" else "list" }
            }
            "agents" -> Group(footer = tr("chat.first.note")) {
                SwitchRow(tr("chat.first"), settings.chatFirst) { settings.chatFirst = it }
                val sizes = listOf("s", "m", "l", "xl")
                SegmentRow(
                    tr("chat.text"),
                    sizes.map { tr("chat.text.$it") },
                    sizes.indexOf(settings.chatText).coerceAtLeast(0),
                ) { settings.chatText = sizes[it] }
            }
            "keyboard" -> Group {
                PushRow(tr("k.keybar"), keyCaps.size.toString()) { onPush("k.keybar") }
                SwitchRow(tr("k.haptics"), settings.hapticKeys) { settings.hapticKeys = it }
                SwitchRow(tr("keyboard.incognito"), settings.incognitoKeyboard) { settings.incognitoKeyboard = it }
            }
            "gestures" -> Group {
                SwitchRow(tr("g.pinch"), settings.pinchZoom) { settings.pinchZoom = it }
                SwitchRow(tr("g.twofinger"), settings.twoFingerHidesKeyboard) { settings.twoFingerHidesKeyboard = it }
                SwitchRow(tr("g.longpress"), settings.longPressSelects) { settings.longPressSelects = it }
            }
            "connection" -> Group {
                SwitchRow(tr("c.autoreconnect"), settings.autoReconnect) { settings.autoReconnect = it }
                val keepAlive = settings.keepAliveSeconds
                StepperRow(
                    tr("c.keepalive"),
                    if (keepAlive == 0) tr("off") else "$keepAlive s",
                    onMinus = { settings.keepAliveSeconds = (settings.keepAliveSeconds - 15).coerceIn(0, 300) },
                    onPlus = { settings.keepAliveSeconds = (settings.keepAliveSeconds + 15).coerceIn(0, 300) },
                )
                SwitchRow(tr("c.background"), settings.keepSessionInBackground) { settings.keepSessionInBackground = it }
            }
            "diagnostics" -> if (showLog != null) {
                Group(footer = tr("showlog.foot")) {
                    SwitchRow(tr("showlog"), showLog.value) {
                        showLog.value = it
                        model?.wantsStats = it || settings.devMode
                    }
                    SwitchRow(tr("devmode"), settings.devMode) {
                        settings.devMode = it
                        model?.wantsStats = showLog.value || it
                    }
                }
            }
            "about" -> AboutContent(onDone) { onPush("licenses") }
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
    GroupedPage(title, onBack = onBack) {
        Group {
            for (option in options) {
                GroupRow(onClick = { onPick(option.value) }) {
                    RowTitle(option.label)
                    if (option.value == selected) DesktopIcon("check", size = 18.dp, tint = LeadBlue)
                }
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
    GroupedPage(tr("theme"), onBack = onBack, scroll = false) {
        GlassSearchField(query, tr("theme.search"), Modifier.padding(horizontal = 16.dp, vertical = 6.dp)) { query = it }
        val colors = groupedColors()
        LazyColumn(Modifier.fillMaxSize().padding(horizontal = 16.dp), contentPadding = PaddingValues(top = 10.dp, bottom = 24.dp)) {
            val all = (if (query.isBlank()) listOf<String?>(null) else emptyList()) + names
            itemsIndexed(all, key = { _, it -> it ?: Schemes.FOLLOW_DESKTOP }) { index, name ->
                // One card, cut at its first and last rows.
                val shape = RoundedCornerShape(
                    topStart = if (index == 0) CARD_RADIUS else 0.dp,
                    topEnd = if (index == 0) CARD_RADIUS else 0.dp,
                    bottomStart = if (index == all.lastIndex) CARD_RADIUS else 0.dp,
                    bottomEnd = if (index == all.lastIndex) CARD_RADIUS else 0.dp,
                )
                Column(Modifier.clip(shape).background(colors.hairline)) {
                    if (index > 0) Spacer(Modifier.height(0.5.dp))
                    if (name == null) {
                        ThemeRow(tr("followhost"), null, settings.schemeName == Schemes.FOLLOW_DESKTOP) {
                            settings.schemeName = Schemes.FOLLOW_DESKTOP
                        }
                    } else {
                        ThemeRow(name, Schemes.background(name), settings.schemeName == name) { settings.schemeName = name }
                    }
                }
            }
        }
    }
}

@Composable
private fun ThemeRow(label: String, color: Color?, chosen: Boolean, onPick: () -> Unit) {
    GroupRow(onClick = onPick) {
        if (color != null) {
            Swatch(color, RoundedCornerShape(5.dp), 22.dp)
        } else {
            DesktopIcon("app-window", size = 22.dp, tint = groupedColors().secondary)
        }
        RowTitle(label)
        if (chosen) DesktopIcon("check", size = 18.dp, tint = LeadBlue)
    }
}

/// What the key bar carries, in its order. Read-only for now: the bar is
/// not editable yet.
@Composable
private fun KeyCapsScreen(onBack: () -> Unit) {
    GroupedPage(tr("k.keybar"), onBack = onBack) {
        Group(footer = tr("k.keybar.foot")) {
            for (cap in keyCaps) {
                GroupRow {
                    Text(cap, color = groupedColors().primary, fontSize = 15.sp, fontFamily = FontFamily.Monospace)
                }
            }
        }
    }
}

// MARK: rows

/// A row that pushes a one-of-these screen and shows the choice.
@Composable
private fun PushRow(title: String, value: String, swatch: Color? = null, onClick: () -> Unit) {
    GroupRow(onClick = onClick) {
        RowTitle(title)
        if (swatch != null) Swatch(swatch, CircleShape, 16.dp)
        RowValue(value, Modifier.widthIn(max = 180.dp))
        RowChevron()
    }
}

@Composable
internal fun SwitchRow(title: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    GroupRow(onClick = { onChange(!checked) }) {
        RowTitle(title)
        IosSwitch(checked, onChange)
    }
}

@Composable
private fun StepperRow(title: String, value: String, onMinus: () -> Unit, onPlus: () -> Unit) {
    GroupRow {
        RowTitle(title)
        RowValue(value)
        IosStepper(onMinus, onPlus)
    }
}

/// The label with the segments under it, in one row.
@Composable
private fun SegmentRow(title: String, options: List<String>, selected: Int, onSelect: (Int) -> Unit) {
    val colors = groupedColors()
    Column(
        Modifier
            .fillMaxWidth()
            .background(colors.card)
            .padding(horizontal = 16.dp, vertical = 10.dp)
    ) {
        Text(title, color = colors.primary, fontSize = 16.sp)
        Spacer(Modifier.height(8.dp))
        IosSegments(options, selected, Modifier.fillMaxWidth(), onSelect)
    }
}

@Composable
private fun Swatch(color: Color, shape: Shape, size: Dp) {
    Box(
        Modifier
            .size(size)
            .clip(shape)
            .background(color)
            .border(1.dp, groupedColors().secondary.copy(alpha = 0.35f), shape)
    )
}

// MARK: the text size

/// The open terminal's text size, stepped live; the size a new
/// connection starts at follows it.
@Composable
private fun LiveTextSize(model: TerminalModel) {
    StepperRow(
        tr("textsize"),
        // The size the buttons step and keep, not the core's word on it,
        // which waits for a connection to say.
        String.format(Locale.US, "%.0f pt", model.settings.fontSize),
        onMinus = { stepText(model, -1.0) },
        onPlus = { stepText(model, 1.0) },
    )
}

private fun stepText(model: TerminalModel, by: Double) {
    // Kept in the settings by the model, as a pinch is.
    model.stepFont(by)
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
