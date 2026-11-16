package com.roversx.thinkterm

import android.content.ClipboardManager
import android.content.Context
import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.net.Uri
import android.provider.OpenableColumns
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.CheckCircle
import androidx.compose.material.icons.outlined.ErrorOutline
import androidx.compose.material.icons.outlined.FileUpload
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.kyant.backdrop.Backdrop
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/// A photo, a file or the clipboard's picture, left on the host with its
/// path put on the command line (`TerminalModel.upload`): a piece of glass
/// beside the keys, as iOS's paperclip.
@Composable
fun AttachButton(model: TerminalModel, glass: Backdrop?, width: Dp, height: Dp, tap: () -> Unit) {
    val context = LocalContext.current
    var menu by remember { mutableStateOf(false) }
    val photos = rememberLauncherForActivityResult(ActivityResultContracts.PickMultipleVisualMedia(5)) { uris ->
        model.picking = false
        for (uri in uris) sendPhoto(context, model, uri)
        if (uris.isNotEmpty()) tap()
    }
    val files = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris ->
        model.picking = false
        for (uri in uris) {
            val name = displayName(context, uri) ?: "file"
            model.upload(name) { copy(context, uri, it) }
        }
        if (uris.isNotEmpty()) tap()
    }
    val chrome = LocalChrome.current
    Box {
        Box(
            Modifier
                .size(width, height)
                .glass(glass, chrome.dark, CircleShape)
                .clickable { menu = true },
            contentAlignment = Alignment.Center,
        ) {
            DesktopIcon("paperclip", size = 19.dp, tint = chrome.ink)
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(text = { Text(tr("attach.photo")) }, onClick = {
                menu = false
                model.picking = true
                photos.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly))
            })
            DropdownMenuItem(text = { Text(tr("attach.file")) }, onClick = {
                menu = false
                model.picking = true
                files.launch(arrayOf("*/*"))
            })
            // Asked whether, not what: reading it is the user's to allow,
            // and the system says so when an app reads it.
            if (clipboardHasPicture(context)) {
                DropdownMenuItem(text = { Text(tr("attach.paste")) }, onClick = {
                    menu = false
                    val clip = clipboardPicture(context)
                    if (clip != null) {
                        model.upload(stamped("Pasted", extension(clip.second, "png"))) { copy(context, clip.first, it) }
                        tap()
                    }
                })
            }
        }
    }
}

/// Files on their way to the host, over the keys: a bar for how far each
/// has gone and a way to give it up; what it came to, for a moment after.
@Composable
fun UploadStrip(model: TerminalModel, glass: Backdrop?) {
    if (model.uploads.isEmpty()) return
    val chrome = LocalChrome.current
    Column(Modifier.padding(horizontal = 10.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
        for (upload in model.uploads) {
            val fraction = if (upload.total > 0uL) upload.sent.toFloat() / upload.total.toFloat() else 0f
            Column(
                Modifier
                    .fillMaxWidth()
                    .glass(glass, chrome.dark, RoundedCornerShape(18.dp))
                    .padding(start = 14.dp, end = if (upload.ended == null) 6.dp else 14.dp, top = 8.dp, bottom = 8.dp),
                verticalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Icon(
                        when {
                            upload.error != null -> Icons.Outlined.ErrorOutline
                            upload.path != null -> Icons.Outlined.CheckCircle
                            else -> Icons.Outlined.FileUpload
                        },
                        contentDescription = null,
                        tint = when {
                            upload.error != null -> Color(0xFFFF453A)
                            upload.path != null -> Color(0xFF34C759)
                            else -> chrome.ink
                        },
                        modifier = Modifier.size(18.dp),
                    )
                    Text(
                        upload.name,
                        color = chrome.ink,
                        fontSize = 14.sp,
                        maxLines = 1,
                        overflow = TextOverflow.MiddleEllipsis,
                        modifier = Modifier.weight(1f),
                    )
                    val error = upload.error
                    if (error != null) {
                        Text(error, color = Color(0xFFFF453A), fontSize = 13.sp, maxLines = 1)
                    } else if (upload.path == null) {
                        Text("${(fraction * 100).toInt()}%", color = chrome.ink.copy(alpha = 0.7f), fontSize = 13.sp)
                        Box(
                            Modifier.size(30.dp).clickable { model.cancelUpload(upload.id) },
                            contentAlignment = Alignment.Center,
                        ) {
                            DesktopIcon("x", size = 14.dp, tint = chrome.ink)
                        }
                    }
                }
                if (upload.ended == null) {
                    LinearProgressIndicator(
                        progress = { fraction },
                        modifier = Modifier.fillMaxWidth().padding(end = 8.dp),
                        color = LeadBlue,
                        trackColor = chrome.ink.copy(alpha = 0.15f),
                        drawStopIndicator = {},
                    )
                }
            }
        }
    }
}

/// "Photo 2026-01-02 at 03.04.05.jpg", as the desktop names a pasted
/// picture ("Pasted ... .png").
private fun stamped(word: String, ext: String): String =
    "$word ${SimpleDateFormat("yyyy-MM-dd 'at' HH.mm.ss", Locale.US).format(Date())}.$ext"

private fun extension(mime: String?, fallback: String): String = when (mime) {
    "image/jpeg" -> "jpg"
    "image/png" -> "png"
    "image/gif" -> "gif"
    "image/webp" -> "webp"
    else -> fallback
}

/// A photo as anything reads it: the camera's HEIF made a JPEG, as iOS
/// hands over the picker's "compatible" encoding; others as they are.
private fun sendPhoto(context: Context, model: TerminalModel, uri: Uri) {
    val mime = context.contentResolver.getType(uri)
    if (mime == "image/heic" || mime == "image/heif") {
        model.upload(stamped("Photo", "jpg")) { file ->
            val bitmap = ImageDecoder.decodeBitmap(ImageDecoder.createSource(context.contentResolver, uri))
            file.outputStream().use { bitmap.compress(Bitmap.CompressFormat.JPEG, 92, it) }
            bitmap.recycle()
        }
    } else {
        model.upload(stamped("Photo", extension(mime, "jpg"))) { copy(context, uri, it) }
    }
}

private fun copy(context: Context, uri: Uri, to: File) {
    val input = context.contentResolver.openInputStream(uri) ?: error("unreadable")
    input.use { from -> to.outputStream().use { from.copyTo(it) } }
}

private fun displayName(context: Context, uri: Uri): String? =
    context.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
        if (c.moveToFirst()) c.getString(0) else null
    }

/// Whether the clipboard holds a picture, from its description alone.
private fun clipboardHasPicture(context: Context): Boolean {
    val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    val description = clipboard.primaryClipDescription ?: return false
    return (0 until description.mimeTypeCount).any { description.getMimeType(it).startsWith("image/") }
}

/// The clipboard's picture: its address and type.
private fun clipboardPicture(context: Context): Pair<Uri, String?>? {
    val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    val uri = clipboard.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.uri ?: return null
    return uri to context.contentResolver.getType(uri)
}
