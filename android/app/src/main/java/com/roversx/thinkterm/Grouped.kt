package com.roversx.thinkterm

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.ContentTransform
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.animation.core.animateDpAsState
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.kyant.backdrop.Backdrop
import com.kyant.backdrop.backdrops.layerBackdrop
import com.kyant.backdrop.backdrops.rememberLayerBackdrop

/// The app's own pages -- the settings, the hosts, a host's editor -- drawn
/// as iOS draws them, not as Material does: a grouped list on the system's
/// grouped ground, rounded cards of rows with hairlines between them, a
/// large title under a glass back button, and iOS's own switches, segments
/// and steppers. From the iOS app's Form pages.

/// The colours of the grouped pages, in the system's lightness. The
/// ground is the threads list's (`listBackdrop`), so going from there to
/// a page keeps the colour behind it; the cards stay a step lighter.
@Immutable
class GroupedColors(val dark: Boolean) {
    val ground = listBackdrop(dark)
    val card = if (dark) Color(0xFF1C1C1E) else Color.White
    val hairline = if (dark) Color(0xFF38383A) else Color(0xFFC6C6C8)
    val primary = if (dark) Color.White else Color.Black
    val secondary = if (dark) Color(0x99EBEBF5) else Color(0x993C3C43)
    val tertiary = if (dark) Color(0x4DEBEBF5) else Color(0x4D3C3C43)
    val fill = if (dark) Color(0x3D767680) else Color(0x1F767680)
    val accent = LeadBlue
    val destructive = if (dark) Color(0xFFFF453A) else Color(0xFFFF3B30)
}

val LocalGrouped = staticCompositionLocalOf<GroupedColors?> { null }

/// The page's colours, or outside one (the key panel, a dialog) the
/// theme's lightness.
@Composable
fun groupedColors(): GroupedColors =
    LocalGrouped.current ?: GroupedColors(MaterialTheme.colorScheme.surface.luminance() < 0.5f)

/// What the page's bar of glass shows through: the content scrolling
/// under it.
private val LocalPageGlass = staticCompositionLocalOf<Backdrop?> { null }

/// What glass inside the content (a search field) shows through: the
/// ground behind it. Not the content: glass drawn from a layer that holds
/// it would draw itself without end.
internal val LocalGroundGlass = staticCompositionLocalOf<Backdrop?> { null }

/// The corners of a page's cards, as round as iOS's grouped lists are.
val CARD_RADIUS = 24.dp

/// A page: the content runs under a bar of glass buttons (back, Done, a
/// page's own), fading there as the threads list does, with the large
/// title at its head; once the title has scrolled under the bar, the bar
/// carries it small, as iOS's navigation bar does. `scroll` false leaves
/// the scrolling to the content (a lazy list), which then starts under the
/// title.
@Composable
fun GroupedPage(
    title: String,
    onBack: (() -> Unit)?,
    onDone: (() -> Unit)? = null,
    leading: (@Composable RowScope.() -> Unit)? = null,
    trailing: (@Composable RowScope.() -> Unit)? = null,
    scroll: Boolean = true,
    content: @Composable ColumnScope.() -> Unit,
) {
    val colors = GroupedColors(MaterialTheme.colorScheme.surface.luminance() < 0.5f)
    val glass = rememberLayerBackdrop()
    val groundGlass = rememberLayerBackdrop()
    val bars = WindowInsets.statusBars.asPaddingValues()
    val foot = WindowInsets.navigationBars.asPaddingValues()
    val scrollState = rememberScrollState()
    val density = LocalDensity.current
    // The large title is under the bar: the bar says it.
    val titled = scroll && scrollState.value > with(density) { 40.dp.toPx() }
    val barHeight = 56.dp
    CompositionLocalProvider(LocalGrouped provides colors, LocalPageGlass provides glass, LocalGroundGlass provides groundGlass) {
        BoxWithConstraints(Modifier.fillMaxSize().background(colors.ground).imePadding()) {
            // On a wide screen the cards keep to a readable width, in the
            // middle, as iOS's grouped lists on an iPad.
            val extra = ((maxWidth - READABLE_WIDTH) / 2).coerceAtLeast(0.dp)
            Box(Modifier.matchParentSize().layerBackdrop(groundGlass).background(colors.ground))
            Column(
                Modifier
                    .fillMaxSize()
                    .layerBackdrop(glass)
                    .background(colors.ground)
                    .then(if (scroll) Modifier.verticalScroll(scrollState) else Modifier)
                    .padding(top = bars.calculateTopPadding() + barHeight, bottom = if (scroll) 24.dp + foot.calculateBottomPadding() else 0.dp)
                    .padding(horizontal = extra),
            ) {
                Text(
                    title,
                    color = colors.primary,
                    fontSize = 32.sp,
                    fontWeight = FontWeight.Bold,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.padding(start = 20.dp, end = 20.dp, bottom = 4.dp),
                )
                if (scroll) {
                    content()
                } else {
                    Column(Modifier.fillMaxWidth().weight(1f)) { content() }
                }
            }
            // The bar, the list blurring and fading under it rather than cut.
            Box(
                Modifier
                    .fillMaxWidth()
                    .progressiveBlur(glass, colors.ground, top = true)
                    .padding(top = bars.calculateTopPadding())
                    .height(barHeight + 16.dp),
            ) {
                Row(
                    Modifier.fillMaxWidth().height(barHeight).padding(horizontal = 14.dp + extra),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    if (onBack != null) GlassIconButton("chevron-left", tr("back"), onBack)
                    leading?.invoke(this)
                    Spacer(Modifier.weight(1f))
                    trailing?.invoke(this)
                    if (onDone != null) GlassTextButton(tr("done"), bold = true, onClick = onDone)
                }
                AnimatedVisibility(
                    titled,
                    enter = fadeIn(),
                    exit = fadeOut(),
                    modifier = Modifier.align(Alignment.TopCenter).height(barHeight).padding(horizontal = 120.dp),
                ) {
                    Box(Modifier.height(barHeight), contentAlignment = Alignment.Center) {
                        Text(title, color = colors.primary, fontSize = 17.sp, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    }
                }
            }
        }
    }
}

/// How a page comes and goes, as iOS's navigation stack moves them: one
/// pushed slides in from the right over the one it leaves, which drifts a
/// third of the way left and dims; going back is the same played backward.
fun pushTransition(forward: Boolean): ContentTransform {
    val spec = tween<IntOffset>(durationMillis = 380, easing = FastOutSlowInEasing)
    val fade = tween<Float>(durationMillis = 380)
    return if (forward) {
        (slideInHorizontally(spec) { it } togetherWith
            (slideOutHorizontally(spec) { -it / 3 } + fadeOut(fade, targetAlpha = 0.6f)))
            .apply { targetContentZIndex = 1f }
    } else {
        ((slideInHorizontally(spec) { -it / 3 } + fadeIn(fade, initialAlpha = 0.6f)) togetherWith
            slideOutHorizontally(spec) { it })
            .apply { targetContentZIndex = -1f }
    }
}

/// A sheet's way in and out: up from the bottom over what it covers.
fun sheetTransition(forward: Boolean): ContentTransform {
    val spec = tween<IntOffset>(durationMillis = 360, easing = FastOutSlowInEasing)
    return if (forward) {
        (slideInVertically(spec) { it } togetherWith fadeOut(tween(360), targetAlpha = 0.85f)).apply { targetContentZIndex = 1f }
    } else {
        (fadeIn(tween(360), initialAlpha = 0.85f) togetherWith slideOutVertically(spec) { it }).apply { targetContentZIndex = -1f }
    }
}

/// A round glass button with one of the desktop's icons.
@Composable
fun GlassIconButton(icon: String, label: String, onClick: () -> Unit) {
    val colors = groupedColors()
    Box(
        Modifier.size(44.dp).glass(LocalPageGlass.current, colors.dark, CircleShape)
            .clickable(role = Role.Button, onClick = onClick)
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) {
        DesktopIcon(icon, tint = colors.primary)
    }
}

/// A glass capsule with words: Done, Save, Cancel.
@Composable
fun GlassTextButton(label: String, bold: Boolean = false, enabled: Boolean = true, onClick: () -> Unit) {
    val colors = groupedColors()
    Box(
        Modifier
            .height(44.dp)
            .glass(LocalPageGlass.current, colors.dark, CircleShape)
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = 16.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            label,
            color = if (enabled) colors.primary else colors.tertiary,
            fontSize = 16.sp,
            fontWeight = if (bold) FontWeight.SemiBold else FontWeight.Normal,
            maxLines = 1,
        )
    }
}

/// A section: its title, a rounded card of rows with hairlines between
/// them, and what it says under the card.
@Composable
fun Group(title: String? = null, footer: String? = null, content: @Composable ColumnScope.() -> Unit) {
    val colors = groupedColors()
    Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp)) {
        if (title != null) {
            Text(
                title.uppercase(),
                color = colors.secondary,
                fontSize = 13.sp,
                modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 22.dp, bottom = 7.dp),
            )
        } else {
            Spacer(Modifier.height(22.dp))
        }
        // The card's own colour shows between the rows, each in the
        // card's colour: a hairline between two, none above or below.
        Column(
            Modifier.fillMaxWidth().clip(RoundedCornerShape(CARD_RADIUS)).background(colors.hairline),
            verticalArrangement = Arrangement.spacedBy(0.5.dp),
            content = content,
        )
        if (footer != null) {
            Text(
                footer,
                color = colors.secondary,
                fontSize = 13.sp,
                modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 7.dp),
            )
        }
    }
}

/// One row of a card: what it holds, at least a tap's height.
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun GroupRow(onClick: (() -> Unit)? = null, onLongClick: (() -> Unit)? = null, content: @Composable RowScope.() -> Unit) {
    val colors = groupedColors()
    Row(
        Modifier
            .fillMaxWidth()
            .background(colors.card)
            .then(
                if (onClick != null || onLongClick != null) {
                    Modifier.combinedClickable(onClick = onClick ?: {}, onLongClick = onLongClick)
                } else Modifier
            )
            .heightIn(min = 46.dp)
            .padding(horizontal = 16.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        content = content,
    )
}

/// The row's own words, in the system's body size.
@Composable
fun RowScope.RowTitle(text: String, color: Color? = null, weight: Boolean = true) {
    Text(
        text,
        color = color ?: groupedColors().primary,
        fontSize = 16.sp,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        modifier = if (weight) Modifier.weight(1f) else Modifier,
    )
}

/// What a row says of its value, quiet, at its end.
@Composable
fun RowValue(text: String, modifier: Modifier = Modifier, align: TextAlign? = null) {
    Text(
        text,
        color = groupedColors().secondary,
        fontSize = 16.sp,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        textAlign = align,
        modifier = modifier,
    )
}

/// The chevron a row that opens another page ends with.
@Composable
fun RowChevron() {
    DesktopIcon("chevron-right", size = 16.dp, tint = groupedColors().tertiary)
}

/// iOS's switch: a green track when on, a white knob that slides.
@Composable
fun IosSwitch(checked: Boolean, onChange: (Boolean) -> Unit) {
    val colors = groupedColors()
    val track by animateColorAsState(if (checked) Color(0xFF34C759) else if (colors.dark) Color(0xFF39393D) else Color(0xFFE9E9EA), label = "track")
    val knob by animateDpAsState(if (checked) 22.dp else 2.dp, label = "knob")
    Box(
        Modifier
            .width(51.dp)
            .height(31.dp)
            .clip(CircleShape)
            .background(track)
            .clickable { onChange(!checked) },
    ) {
        Box(
            Modifier
                .offset(x = knob, y = 2.dp)
                .size(27.dp)
                .shadow(2.dp, CircleShape)
                .background(Color.White, CircleShape)
        )
    }
}

/// iOS's segmented control: the choices in a quiet track, the chosen one
/// raised in a card of its own.
@Composable
fun IosSegments(options: List<String>, selected: Int, modifier: Modifier = Modifier, onSelect: (Int) -> Unit) {
    val colors = groupedColors()
    Row(
        modifier.height(32.dp).clip(RoundedCornerShape(9.dp)).background(colors.fill).padding(2.dp),
        horizontalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        options.forEachIndexed { index, label ->
            val on = index == selected
            Box(
                Modifier
                    .weight(1f)
                    .height(28.dp)
                    .then(if (on) Modifier.shadow(1.dp, RoundedCornerShape(7.dp)) else Modifier)
                    .clip(RoundedCornerShape(7.dp))
                    .background(if (on) (if (colors.dark) Color(0xFF636366) else Color.White) else Color.Transparent)
                    .clickable { onSelect(index) },
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    label,
                    color = colors.primary,
                    fontSize = 13.sp,
                    fontWeight = if (on) FontWeight.SemiBold else FontWeight.Normal,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.padding(horizontal = 6.dp),
                )
            }
        }
    }
}

/// iOS's stepper: minus and plus in one quiet capsule, split by a line.
@Composable
fun IosStepper(onMinus: () -> Unit, onPlus: () -> Unit) {
    val colors = groupedColors()
    Row(
        Modifier.height(32.dp).clip(RoundedCornerShape(9.dp)).background(colors.fill),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.width(47.dp).height(32.dp).clickable(onClick = onMinus), contentAlignment = Alignment.Center) {
            Text("−", color = colors.primary, fontSize = 20.sp)
        }
        Box(Modifier.width(1.dp).height(18.dp).background(colors.tertiary))
        Box(Modifier.width(47.dp).height(32.dp).clickable(onClick = onPlus), contentAlignment = Alignment.Center) {
            Text("+", color = colors.primary, fontSize = 20.sp)
        }
    }
}

/// A row with a label and the field it names, as a Form's text field row.
@Composable
fun FieldRow(
    label: String,
    value: String,
    onChange: (String) -> Unit,
    placeholder: String = "",
    keyboard: KeyboardType = KeyboardType.Text,
    secret: Boolean = false,
    monospace: Boolean = false,
    singleLine: Boolean = true,
    minLines: Int = 1,
    enabled: Boolean = true,
) {
    val colors = groupedColors()
    GroupRow {
        if (label.isNotEmpty()) {
            Text(label, color = colors.primary, fontSize = 16.sp, maxLines = 1, modifier = Modifier.widthIn(min = 96.dp, max = 140.dp))
        }
        Box(Modifier.weight(1f).padding(vertical = 6.dp)) {
            if (value.isEmpty()) {
                Text(placeholder, color = colors.tertiary, fontSize = 16.sp, maxLines = if (singleLine) 1 else 4)
            }
            BasicTextField(
                value = value,
                onValueChange = onChange,
                enabled = enabled,
                singleLine = singleLine,
                minLines = minLines,
                textStyle = TextStyle(
                    color = colors.primary,
                    fontSize = if (monospace) 13.sp else 16.sp,
                    fontFamily = if (monospace) FontFamily.Monospace else FontFamily.Default,
                ),
                cursorBrush = SolidColor(colors.accent),
                // Nothing here is prose: no capitals of its own, no autocorrect.
                keyboardOptions = KeyboardOptions(
                    keyboardType = keyboard,
                    capitalization = androidx.compose.ui.text.input.KeyboardCapitalization.None,
                    autoCorrectEnabled = false,
                ),
                visualTransformation = if (secret) androidx.compose.ui.text.input.PasswordVisualTransformation() else VisualTransformation.None,
                modifier = Modifier.fillMaxWidth(),
            )
        }
    }
}

/// A search field in glass over the page's ground.
@Composable
fun GlassSearchField(query: String, placeholder: String, modifier: Modifier = Modifier, onChange: (String) -> Unit) {
    val colors = groupedColors()
    Row(
        modifier
            .fillMaxWidth()
            .height(40.dp)
            .glass(LocalGroundGlass.current, colors.dark, CircleShape)
            .padding(horizontal = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        DesktopIcon("search", size = 18.dp, tint = colors.secondary)
        Spacer(Modifier.width(8.dp))
        Box(Modifier.weight(1f)) {
            if (query.isEmpty()) Text(placeholder, color = colors.tertiary, fontSize = 16.sp, maxLines = 1)
            BasicTextField(
                value = query,
                onValueChange = onChange,
                singleLine = true,
                textStyle = TextStyle(color = colors.primary, fontSize = 16.sp),
                cursorBrush = SolidColor(colors.accent),
                keyboardOptions = KeyboardOptions(autoCorrectEnabled = false),
                modifier = Modifier.fillMaxWidth(),
            )
        }
    }
}

/// A field in a dialog: a quiet rounded box, no Material outline.
@Composable
fun DialogField(value: String, onChange: (String) -> Unit, placeholder: String, monospace: Boolean = false, singleLine: Boolean = true) {
    val colors = groupedColors()
    Box(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(10.dp))
            .background(colors.fill)
            .padding(horizontal = 12.dp, vertical = 10.dp)
    ) {
        val family = if (monospace) FontFamily.Monospace else FontFamily.Default
        if (value.isEmpty()) Text(placeholder, color = colors.tertiary, fontSize = 15.sp, fontFamily = family, maxLines = 1)
        BasicTextField(
            value = value,
            onValueChange = onChange,
            singleLine = singleLine,
            maxLines = if (singleLine) 1 else 6,
            textStyle = TextStyle(color = colors.primary, fontSize = 15.sp, fontFamily = family),
            cursorBrush = SolidColor(colors.accent),
            keyboardOptions = KeyboardOptions(
                capitalization = androidx.compose.ui.text.input.KeyboardCapitalization.None,
                autoCorrectEnabled = false,
            ),
            modifier = Modifier.fillMaxWidth(),
        )
    }
}

/// A dialog's field for one line to edit, as `DialogField` draws it, with
/// the keys on it as it shows: what `value` selects stays selected, so a
/// name given to start from is replaced by the first key typed.
@Composable
fun DialogField(value: TextFieldValue, onChange: (TextFieldValue) -> Unit, placeholder: String) {
    val colors = groupedColors()
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { focus.requestFocus() }
    Box(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(10.dp))
            .background(colors.fill)
            .padding(horizontal = 12.dp, vertical = 10.dp)
    ) {
        if (value.text.isEmpty()) Text(placeholder, color = colors.tertiary, fontSize = 15.sp, maxLines = 1)
        BasicTextField(
            value = value,
            onValueChange = onChange,
            singleLine = true,
            textStyle = TextStyle(color = colors.primary, fontSize = 15.sp),
            cursorBrush = SolidColor(colors.accent),
            keyboardOptions = KeyboardOptions(
                capitalization = androidx.compose.ui.text.input.KeyboardCapitalization.None,
                autoCorrectEnabled = false,
            ),
            modifier = Modifier.fillMaxWidth().focusRequester(focus),
        )
    }
}

/// A capsule button: the blue one that leads, or a quiet one in `ink`.
@Composable
fun CapsuleButton(label: String, lead: Boolean, ink: Color, enabled: Boolean = true, onClick: () -> Unit) {
    Box(
        Modifier
            .height(40.dp)
            .clip(CircleShape)
            .background(if (lead) LeadBlue else ink.copy(alpha = 0.12f))
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = 18.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, color = if (lead) Color.White else ink, fontSize = 15.sp, fontWeight = FontWeight.SemiBold, maxLines = 1)
    }
}
