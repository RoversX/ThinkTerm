//! Attaching the pictures serialized lines name, fetching the ones not
//! held, and keeping a held copy in step with the original.
use crate::clock::Clock;
use crate::host::{request, SessionHost};
use crate::images::{file_image, frame_count, merge_into, ImageStore};
use crate::Lock;
use codec::{GetImageCell, GetImageCellResponse, Pdu, SerializedLines};
use std::collections::HashMap;
use std::sync::Arc;
use termwiz::image::{ImageCell, ImageData};
use thinkterm_proto::PaneId;
use wezterm_term::{Line, StableRowIndex};

/// Attach the images the serialized lines name. With `fetch_images` off,
/// nothing is asked of the server: images already held are attached, and
/// rows whose pictures would have to be fetched are left out, so the row
/// the cache already shows stays -- the previous frame rather than a
/// blank. Used for a push that a newer push has already overtaken. The
/// rows left out are returned with the lines: the caller has to see that
/// something brings them, or they would stay as they were for good, since
/// the push carried them as bonus rows and nothing marks those dirty.
pub async fn hydrate_lines<H: SessionHost>(
    host: &H,
    images: &Lock<ImageStore>,
    pane_id: PaneId,
    serialized_lines: SerializedLines,
    fetch_images: bool,
) -> (Vec<(StableRowIndex, Line)>, Vec<StableRowIndex>) {
    let (mut lines, mut image_cells) = serialized_lines.extract_data();
    host.resolve_image_cells(pane_id, &mut lines, &mut image_cells);

    if image_cells.is_empty() {
        return (lines, vec![]);
    }

    let mut requests = HashMap::new();
    let mut data_by_hash = HashMap::new();
    let domain = host.image_domain();
    images.lock().touch(host.clock().now());
    for im in &image_cells {
        let held = images.lock().get(&(domain, im.data_hash));
        match held {
            // A copy at or past the generation the cell was sent with is
            // current. An animation grows behind an unchanging hash, so
            // the hash alone would say "have it" forever.
            Some(data) if data.generation() >= host.image_generation(pane_id, im) => {
                data_by_hash.insert(im.data_hash, data);
            }
            held => {
                requests.entry(im.data_hash).or_insert_with(|| {
                    let have_frames = held
                        .as_ref()
                        .map(|data| frame_count(&data.data()))
                        .unwrap_or(0);
                    (
                        held,
                        im.image_id,
                        GetImageCell {
                            pane_id,
                            line_idx: im.line_idx,
                            cell_idx: im.cell_idx,
                            data_hash: im.data_hash,
                            data_generation: host.image_generation(pane_id, im),
                            have_frames,
                        },
                    )
                });
            }
        }
    }

    let mut left_out = vec![];
    if !fetch_images && !requests.is_empty() {
        let unfetched: std::collections::HashSet<StableRowIndex> = image_cells
            .iter()
            .filter(|im| requests.contains_key(&im.data_hash))
            .map(|im| im.line_idx)
            .collect();
        lines.retain(|(idx, _)| !unfetched.contains(idx));
        left_out = unfetched.into_iter().collect();
        requests.clear();
    }

    // Concurrently, not one at a time: these are independent round trips, so
    // awaiting them serially cost a line with N distinct images N times the
    // latency.
    let asked: Vec<([u8; 32], Option<Arc<ImageData>>, Option<u32>, GetImageCell)> = requests
        .into_iter()
        .map(|(hash, (held, image_id, request))| (hash, held, image_id, request))
        .collect();
    let fetched = futures_util::future::join_all(
        asked
            .iter()
            .map(|(_, held, image_id, request)| fetch_image(host, held.clone(), request.clone(), *image_id)),
    )
    .await;

    // A reconnect can finish while these requests are in flight. Its new
    // image domain must not inherit the predecessor's pixels or generation.
    if host.image_domain() != domain {
        return (Vec::new(), lines.iter().map(|(idx, _)| *idx).collect());
    }

    for ((asked_for, _, _, _), data) in asked.into_iter().zip(fetched) {
        let Some(data) = data else { continue };
        let data = file_image(images, domain, data);
        // Filed under the hash the cell named as well: the server may have
        // answered with the picture now in the cell, whose hash differs.
        data_by_hash.insert(asked_for, Arc::clone(&data));
        data_by_hash.insert(data.hash(), data);
    }

    let mut line_by_idx = HashMap::new();
    for (line_idx, line) in lines {
        line_by_idx.insert(line_idx, line);
    }

    for im in image_cells {
        if let Some(data) = data_by_hash.get(&im.data_hash) {
            if let Some(line) = line_by_idx.get_mut(&im.line_idx) {
                if let Some(cell) = line.cells_mut_for_attr_changes_only().get_mut(im.cell_idx) {
                    cell.attrs_mut()
                        .attach_image(Box::new(ImageCell::with_z_index(
                            im.top_left,
                            im.bottom_right,
                            Arc::clone(data),
                            im.z_index,
                            im.padding_left,
                            im.padding_top,
                            im.padding_right,
                            im.padding_bottom,
                            im.image_id,
                            im.placement_id,
                        )));
                }
            }
        }
    }

    (line_by_idx.into_iter().collect(), left_out)
}

async fn get_image_cell<H: SessionHost>(
    host: &H,
    req: GetImageCell,
    image_id: Option<u32>,
) -> anyhow::Result<GetImageCellResponse> {
    let pdu = host.image_request(req.clone(), image_id);
    let canonical = matches!(pdu, Pdu::GetKittyImage(_));
    let extract = |pdu| match pdu {
        Pdu::GetImageCellResponse(response) => Ok(response),
        other => Err(other),
    };
    match request(host.link(), pdu, extract).await {
        // The cell can answer where the image id could not: a proxy whose
        // upstream predates fetching by id refuses every such request.
        Err(err) if canonical => {
            log::debug!("fetching an image by id failed ({err:#}); asking by its cell");
            request(host.link(), Pdu::GetImageCell(req), extract).await
        }
        answer => answer,
    }
}

/// Fetch the image `request` names and bring `held`, the copy already
/// filed under that hash, up to date in place; the Arc to file is returned.
/// A delta the copy cannot take (its leading frames no longer match) is
/// followed by one fetch of the whole image. A copy is never replaced by
/// a fresh Arc: the glyph cache is keyed by hash and holds the copy, so a
/// second Arc under the same hash would leave the painter on the old one
/// for good. A whole image the copy still will not take (it would shrink
/// under a painter) leaves the copy as it is.
pub(crate) async fn fetch_image<H: SessionHost>(
    host: &H,
    held: Option<Arc<ImageData>>,
    request: GetImageCell,
    image_id: Option<u32>,
) -> Option<Arc<ImageData>> {
    use crate::clock::Clock;
    let whole = GetImageCell {
        have_frames: 0,
        ..request
    };
    let domain = host.image_domain();
    let canonical = matches!(host.image_request(whole.clone(), image_id), Pdu::GetKittyImage(_));
    let asked_at = host.clock().now();
    let mut response = get_image_cell(host, request, image_id).await;
    if log::log_enabled!(log::Level::Debug) {
        let bytes = match &response {
            Ok(GetImageCellResponse {
                data: Some(data), ..
            }) => data.len(),
            _ => 0,
        };
        log::debug!(
            "image fetch for pane {} took {:?} ({bytes} bytes)",
            whole.pane_id,
            host.clock().now().saturating_duration_since(asked_at)
        );
    }
    let mut asked_for_whole = false;
    loop {
        if host.image_domain() != domain { return None; }
        match response {
            Ok(GetImageCellResponse {
                data: Some(fresh),
                data_generation,
                frames_from,
                ..
            }) => {
                // The wire built this field by field; nothing between it
                // and the GPU checks the buffer against the size it claims.
                if !fresh.data().is_well_formed_tail(frames_from) {
                    log::warn!(
                        "the server sent an image whose pixel data does not match its \
                         declared size; ignoring it"
                    );
                    return None;
                }
                if fresh.hash() != whole.data_hash {
                    // Not the picture asked for but the one now in the cell:
                    // a newer frame. Its own copy, never merged into the
                    // one held for the old hash. It is always sent whole;
                    // a tail of it would be frames counted from a copy of
                    // some other picture, and adopting it as the whole
                    // would leave a truncated animation under its hash.
                    if frames_from != 0 {
                        log::warn!(
                            "the server answered a fetch for one image with part of another; \
                             ignoring it"
                        );
                        return None;
                    }
                    fresh.set_generation(data_generation);
                    return Some(fresh);
                }
                if canonical && held.as_ref().is_some_and(|data| data.generation() > data_generation) {
                    return held;
                }
                match &held {
                    None if frames_from == 0 => {
                        fresh.set_generation(data_generation);
                        return Some(fresh);
                    }
                    Some(held) if merge_into(held, &fresh, frames_from, data_generation) => {
                        return Some(Arc::clone(held));
                    }
                    _ => {}
                }
                if !asked_for_whole {
                    asked_for_whole = true;
                    log::debug!("image delta did not fit the copy held; fetching the whole image");
                    response = get_image_cell(host, GetImageCell { ..whole }, image_id).await;
                    continue;
                }
                // The copy stays, and is marked as current as the server's:
                // left at its old generation, every later push would find
                // it behind and fetch the whole image again, for good.
                log::debug!("the whole image did not fit the copy held either; keeping the copy");
                if let Some(held) = &held {
                    held.set_generation(data_generation);
                }
                return held;
            }
            Ok(GetImageCellResponse { data: None, .. }) => {
                // Not an error: the image has been let go of on the server
                // by the time the request lands, which is the ordinary
                // outcome for a pane streaming frames faster than the round
                // trip. This cell renders without it and the next frame
                // supersedes it.
                log::debug!("image cell no longer holds the requested hash");
                return None;
            }
            Err(err) => {
                log::error!("failed to retrieve image {err:#}");
                return None;
            }
        }
    }
}
