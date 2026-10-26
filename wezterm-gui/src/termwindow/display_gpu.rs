//! Prefer the GPU that drives a connected display when the WebGPU front end
//! would otherwise pick one that cannot present.
//!
//! wgpu's `LowPower` preference (the default) asks for an integrated GPU when
//! one is present. On a hybrid system whose panel is wired to the discrete
//! GPU -- some laptops are laid out that way, and so are systems where the
//! compositor runs on the discrete GPU -- the integrated adapter still reports
//! the surface as compatible, renders every frame, but the compositor never
//! presents them, leaving a black window. `is_surface_supported` does not
//! catch this because the Vulkan surface itself is valid; it is the cross-GPU
//! buffer hand-off that fails.
//!
//! This fallback is deliberately narrow. It only switches away from the
//! adapter wgpu picked when that adapter matches no DRM card with a connected
//! connector, and only to a surface-compatible adapter that does. When the
//! sysfs topology is unavailable or inconclusive the original choice stands,
//! and a configured `webgpu_preferred_adapter` always wins.
//!
//! The compositor's DMA-BUF main device would be the precise signal, but the
//! Wayland layer does not bind `zwp_linux_dmabuf_v1` feedback yet; matching
//! connected connectors is a contained approximation that fixes the common
//! panel-on-the-discrete-GPU topology without changing anything elsewhere.

use std::path::Path;

/// PCI (vendor, device) IDs, as reported by both sysfs and wgpu.
pub(crate) type PciId = (u32, u32);

/// Returns the PCI IDs of DRM cards that have at least one connected
/// connector below `sysfs_drm` (normally `/sys/class/drm`).
pub(crate) fn pci_ids_with_connected_displays(sysfs_drm: &Path) -> Vec<PciId> {
    use std::collections::BTreeSet;

    let Ok(entries) = std::fs::read_dir(sysfs_drm) else {
        return Vec::new();
    };

    let mut cards = BTreeSet::new();
    for entry in entries.flatten() {
        // Connector entries are named `<card>-<connector>`, for example
        // `card2-eDP-2`. Everything else (`card2`, `renderD128`, `version`)
        // has no hyphen and is skipped.
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some((card, _connector)) = name.split_once('-') else {
            continue;
        };
        if !card.starts_with("card") {
            continue;
        }
        let status = std::fs::read_to_string(entry.path().join("status")).unwrap_or_default();
        if status.trim() == "connected" {
            cards.insert(card.to_string());
        }
    }

    let mut ids = Vec::new();
    for card in cards {
        let device = sysfs_drm.join(card).join("device");
        if let (Some(vendor), Some(device)) = (
            read_hex_u32(&device.join("vendor")),
            read_hex_u32(&device.join("device")),
        ) {
            if !ids.contains(&(vendor, device)) {
                ids.push((vendor, device));
            }
        }
    }
    ids
}

fn read_hex_u32(path: &Path) -> Option<u32> {
    let value = std::fs::read_to_string(path).ok()?;
    let value = value.trim();
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    u32::from_str_radix(value, 16).ok()
}

/// Replaces `adapter` when it drives no connected display but another adapter
/// that supports `surface` does. This is a no-op on systems where the choice
/// is already correct or the display topology cannot be read.
pub(crate) fn prefer_display_gpu(
    instance: &wgpu::Instance,
    backends: wgpu::Backends,
    surface: &wgpu::Surface<'_>,
    adapter: &mut wgpu::Adapter,
) {
    let connected = pci_ids_with_connected_displays(Path::new("/sys/class/drm"));
    if connected.is_empty() {
        return;
    }

    let current = adapter.get_info();
    // A zero ID means the backend did not report PCI IDs (some GL paths do
    // not). There is nothing to match against sysfs, so leave the choice be.
    if current.vendor == 0 || current.device == 0 {
        return;
    }
    if connected.contains(&(current.vendor, current.device)) {
        return;
    }

    let mut fallback = None;
    for candidate in instance.enumerate_adapters(backends) {
        let info = candidate.get_info();
        if !connected.contains(&(info.vendor, info.device)) {
            continue;
        }
        if !candidate.is_surface_supported(surface) {
            continue;
        }
        if info.backend == current.backend {
            fallback = Some(candidate);
            break;
        }
        fallback.get_or_insert(candidate);
    }

    let Some(candidate) = fallback else {
        return;
    };

    let info = candidate.get_info();
    log::info!(
        "WebGPU adapter '{}' ({:?}) drives no connected display; \
         using '{}' ({:?}) instead",
        current.name,
        current.device_type,
        info.name,
        info.device_type,
    );
    *adapter = candidate;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn reports_only_cards_with_a_connected_connector() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();

        // Integrated GPU: present, but nothing is plugged into it.
        write(&root.join("card1/device/vendor"), "0x1002\n");
        write(&root.join("card1/device/device"), "0x164e\n");
        write(&root.join("card1-eDP-1/status"), "disconnected\n");
        write(&root.join("card1-DP-1/status"), "disconnected\n");

        // Discrete GPU: drives the panel.
        write(&root.join("card2/device/vendor"), "0x10de\n");
        write(&root.join("card2/device/device"), "0x2860\n");
        write(&root.join("card2-eDP-2/status"), "connected\n");

        // Not connectors; must not contribute a PCI ID.
        write(&root.join("renderD128/device/vendor"), "0x1002\n");
        write(&root.join("renderD128/device/device"), "0x164e\n");
        write(&root.join("version"), "drm 1.1.0\n");

        assert_eq!(
            pci_ids_with_connected_displays(root),
            vec![(0x10de, 0x2860)]
        );
    }

    #[test]
    fn treats_unreadable_sysfs_as_inconclusive() {
        assert!(
            pci_ids_with_connected_displays(Path::new("/nonexistent/thinkterm-display-gpu"))
                .is_empty()
        );
    }
}
