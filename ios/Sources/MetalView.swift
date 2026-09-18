import SwiftUI
import UIKit
import QuartzCore

/// A UIView whose backing layer is the CAMetalLayer the core draws on. The
/// view reports its layer and drawable size to the model on every layout,
/// and takes the surface back before the layer is torn down.
final class MetalLayerView: UIView {
    override class var layerClass: AnyClass { CAMetalLayer.self }
    weak var model: ProbeModel?

    var metalLayer: CAMetalLayer { layer as! CAMetalLayer }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if let window {
            metalLayer.contentsScale = window.screen.scale
            report()
        } else {
            model?.surfaceGone()
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        report()
    }

    private func report() {
        guard let model, window != nil else { return }
        let scale = Double(metalLayer.contentsScale)
        let w = Int((Double(bounds.width) * scale).rounded())
        let h = Int((Double(bounds.height) * scale).rounded())
        guard w > 0, h > 0 else { return }
        // wgpu sets drawableSize on configure; keeping the layer's own value
        // in step avoids a one-frame stretch on rotation.
        metalLayer.drawableSize = CGSize(width: w, height: h)
        model.surfaceReady(metalLayer, width: w, height: h, scale: scale)
    }
}

struct MetalView: UIViewRepresentable {
    @EnvironmentObject var model: ProbeModel

    func makeUIView(context: Context) -> MetalLayerView {
        let view = MetalLayerView()
        view.model = model
        view.backgroundColor = .black
        return view
    }

    func updateUIView(_ uiView: MetalLayerView, context: Context) {}

    static func dismantleUIView(_ uiView: MetalLayerView, coordinator: ()) {
        uiView.model?.surfaceGone()
    }
}
