import SwiftUI

/// A terminal's sidebar: the same list Threads home is (`SpaceList`) --
/// one Space, of any host, picked at its head or swiped to -- with the
/// overview's row and the way back to Threads. On a phone held upright it
/// is the drawer the screen slides off; on a wide screen it stays at the
/// left. A thread or a Space of another host takes the terminal there.
struct SidebarPanel: View {
    /// The terminal's session, whose Space the sidebar shows.
    let model: TerminalModel
    /// Something was picked: a drawer closes, a column stays.
    var done: () -> Void
    var onEditHost: () -> Void
    var onSettings: () -> Void
    var onOverview: () -> Void
    /// Back to Threads.
    var onLeave: () -> Void
    /// Another host's terminal, for a thread or a Space of it picked here;
    /// for a Space, with the sidebar out.
    var onHost: ((UUID, Bool) -> Void)?
    /// The overview is up: its row is lit.
    var overviewOn = false
    /// Someone sees it: a drawer shut under the screen makes no rows, and
    /// keeps its folds and search for when it opens.
    var live = true

    var body: some View {
        let own = model.host?.id ?? Sessions.probeId
        SpaceList(
            place: .panel,
            host: own,
            onOpen: { id, keepPanel in
                if id != own { onHost?(id, keepPanel) }
            },
            onSettings: onSettings,
            onBack: onLeave,
            onEditHost: onEditHost,
            overview: (on: overviewOn, tap: onOverview),
            done: done,
            inFront: model,
            live: live
        )
    }
}

/// One row of the sidebar: an icon or a status mark, the title, an
/// optional fold chevron and trailing button; a rounded fill when it is
/// the one on show. Its text follows the system's size.
/// One of the desktop's icons (`ios/build.sh` makes the catalog), tinted
/// with the text colour round it.
struct DesktopIcon: View {
    var name: String
    var size: CGFloat = 20

    var body: some View {
        Image("lucide-" + name)
            .renderingMode(.template)
            .resizable()
            .scaledToFit()
            .frame(width: size, height: size)
    }
}

/// A maker's mark (`ios/build.sh` makes these from third_party/simple-icons,
/// as the desktop's icons.rs has them), in one colour as `DesktopIcon` is.
struct BrandIcon: View {
    var name: String
    var size: CGFloat = 20

    var body: some View {
        Image("brand-" + name)
            .renderingMode(.template)
            .resizable()
            .scaledToFit()
            .frame(width: size, height: size)
    }
}

struct SidebarRow: View {
    /// A desktop icon's name (`DesktopIcon`).
    var icon: String?
    /// What leads the row in the icon's place: a thread's program cards.
    var lead: AnyView?
    /// A thread's mark, at the row's end.
    var mark: AnyView?
    var title: String
    var subtitle: String?
    var depth = 0
    var bold = false
    var dim = false
    var selected = false
    /// Unfolded (true), folded (false), or nothing to fold (nil).
    var chevron: Bool?
    var trailing: AnyView?
    /// More under the subtitle, in the text's column: an agent's progress
    /// and sub-tasks. The row then grows down from its first line, which
    /// keeps the lead and the end in line with it.
    var below: AnyView?
    var action: () -> Void

    init(
        icon: String? = nil, lead: AnyView? = nil, mark: AnyView? = nil, title: String, subtitle: String? = nil,
        depth: Int = 0, bold: Bool = false, dim: Bool = false, selected: Bool = false,
        chevron: Bool? = nil, trailing: AnyView? = nil, below: AnyView? = nil, action: @escaping () -> Void
    ) {
        self.icon = icon
        self.lead = lead
        self.mark = mark
        self.title = title
        self.subtitle = subtitle
        self.depth = depth
        self.bold = bold
        self.dim = dim
        self.selected = selected
        self.chevron = chevron
        self.trailing = trailing
        self.below = below
        self.action = action
    }

    var body: some View {
        // With something below, each piece is centred on the first line's
        // height rather than on the whole row's.
        let line: CGFloat? = below == nil ? nil : 44
        HStack(alignment: below == nil ? .center : .top, spacing: 12) {
            Group {
                if let lead {
                    lead
                } else if let icon {
                    DesktopIcon(name: icon).foregroundColor(.secondary)
                }
            }
            // A thread's cards keep the room the core says a row keeps.
            .frame(width: lead == nil ? 24 : nil)
            .frame(minHeight: line)
            VStack(alignment: .leading, spacing: 1) {
                VStack(alignment: .leading, spacing: 1) {
                    Text(title)
                        .font(.body.weight(bold || lead != nil ? .semibold : .regular))
                        .foregroundColor(dim ? .secondary : .primary)
                        .lineLimit(1)
                    if let subtitle {
                        Text(subtitle).font(.footnote).foregroundColor(.secondary).lineLimit(1)
                    }
                }
                .frame(minHeight: line)
                if let below { below }
            }
            Spacer(minLength: 0)
            if let trailing { trailing.frame(minHeight: line) }
            // A thread's mark ends its row, as on the desktop.
            if let mark { mark.padding(.trailing, 4).frame(minHeight: line) }
            if let chevron {
                DesktopIcon(name: "chevron-right", size: 14)
                    .foregroundColor(.secondary)
                    .rotationEffect(.degrees(chevron ? 90 : 0))
                    .frame(minHeight: line)
            }
        }
        .padding(.leading, 12 + CGFloat(depth) * 18)
        .padding(.trailing, 10)
        .frame(minHeight: 44)
        .selectedPill(selected)
        .contentShape(Rectangle())
        .onTapGesture(perform: action)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isButton)
    }
}

extension Color {
    /// The ground of the app's own lists -- the threads, the sidebar --
    /// in the system's appearance.
    static let chromeBackdrop = Color(UIColor { traits in
        traits.userInterfaceStyle == .dark ? UIColor(white: 0.07, alpha: 1) : UIColor(white: 0.95, alpha: 1)
    })
    /// The ground of the pages pushed over them: the lists' own in the
    /// dark, so going from the threads to Settings or the hosts keeps the
    /// colour behind them, and the system's grouped ground in the light,
    /// all but the same as theirs.
    static let pageBackdrop = Color(UIColor { traits in
        traits.userInterfaceStyle == .dark ? UIColor(white: 0.07, alpha: 1) : .systemGroupedBackground
    })
}

extension View {
    /// A page's list on `pageBackdrop` rather than the system's ground.
    func onPageBackdrop() -> some View {
        scrollContentBackground(.hidden)
            .background(Color.pageBackdrop.ignoresSafeArea())
    }

    /// A field an alert opens with `name` in it has all of it selected as
    /// the keyboard comes up, so the first key typed replaces it: a
    /// rename's field, which starts from the current name. None, or an
    /// empty name, selects nothing.
    func selectsOnEditing(_ name: String?) -> some View {
        onReceive(NotificationCenter.default.publisher(for: UITextField.textDidBeginEditingNotification)) { note in
            guard let name, !name.isEmpty, let field = note.object as? UITextField, field.text == name else { return }
            // Once the field has put its caret at the end.
            DispatchQueue.main.async {
                field.selectedTextRange = field.textRange(from: field.beginningOfDocument, to: field.endOfDocument)
            }
        }
    }
}

extension View {
    /// The row on show: the desktop's pill, in the system's Liquid Glass
    /// from iOS 26 and as a faint fill with an edge before it.
    @ViewBuilder
    func selectedPill(_ selected: Bool) -> some View {
        if !selected {
            self
        } else if #available(iOS 26.0, *) {
            glassEffect(.regular, in: .capsule)
        } else {
            background(
                Capsule(style: .continuous)
                    .fill(Color.primary.opacity(0.13))
                    .overlay(Capsule(style: .continuous).strokeBorder(Color.primary.opacity(0.1)))
            )
        }
    }

    /// A panel floating over the terminal, in the system's Liquid Glass
    /// from iOS 26 and a material before it.
    @ViewBuilder
    func glassPanel(radius: CGFloat) -> some View {
        if #available(iOS 26.0, *) {
            glassEffect(.regular, in: .rect(cornerRadius: radius))
        } else {
            background(.regularMaterial, in: RoundedRectangle(cornerRadius: radius, style: .continuous))
        }
    }

    /// The system's Liquid Glass round a button, from iOS 26; a material
    /// before it. Tinted, a button that is on.
    @ViewBuilder
    func glassCircle(tint: Color? = nil) -> some View {
        if #available(iOS 26.0, *) {
            if let tint {
                glassEffect(.regular.tint(tint).interactive(), in: .circle)
            } else {
                glassEffect(.regular.interactive(), in: .circle)
            }
        } else if let tint {
            background(tint, in: Circle())
        } else {
            background(.ultraThinMaterial, in: Circle())
        }
    }

    /// The same, in a capsule (a field, a row of buttons); tinted, the one
    /// button that leads.
    @ViewBuilder
    func glassCapsule(tint: Color? = nil) -> some View {
        if #available(iOS 26.0, *) {
            if let tint {
                glassEffect(.regular.tint(tint).interactive(), in: .capsule)
            } else {
                glassEffect(.regular, in: .capsule)
            }
        } else if let tint {
            background(tint, in: Capsule())
        } else {
            background(.ultraThinMaterial, in: Capsule())
        }
    }

    /// A bar at one end of a scrolling list that the list runs under,
    /// fading there rather than cut: the system's soft scroll edge from
    /// iOS 26, a fade into `color` before it. A bar whose words must stay
    /// readable over the list (`scrim`) keeps the fade on every version.
    func floatingBar<Bar: View>(
        edge: VerticalEdge, fading color: Color, scrim: Bool = false, @ViewBuilder _ bar: () -> Bar
    ) -> some View {
        modifier(FloatingBar(edge: edge, color: color, scrim: scrim, bar: bar()))
    }

    /// Reads where the system puts a bar `height` tall along `edge` of the
    /// screen: how far that is from the safe area's edges (`BarRoom`), for
    /// a bar there to add to its margins -- at the sides, a bar that
    /// reaches that side of the screen.
    func barRoom(_ edge: VerticalEdge, height: CGFloat, _ room: Binding<EdgeInsets>) -> some View {
        background(BarRoom(edge: edge, height: height, room: room))
    }

    /// Glass shapes side by side, drawn together as the system draws its
    /// own groups of buttons, from iOS 26.
    @ViewBuilder
    func glassGroup(spacing: CGFloat) -> some View {
        if #available(iOS 26.0, *) {
            GlassEffectContainer(spacing: spacing) { self }
        } else {
            self
        }
    }

    /// A tab's capsule in the glass of the buttons beside it, the tab on
    /// show lit with the bar's ink.
    @ViewBuilder
    func glassPill(lit: Bool, ink: Color) -> some View {
        if #available(iOS 26.0, *) {
            glassEffect(lit ? .regular.tint(ink.opacity(0.2)).interactive() : .regular.interactive(), in: .capsule)
        } else {
            background(ink.opacity(lit ? 0.18 : 0.06), in: Capsule())
        }
    }
}

private struct FloatingBar<Bar: View>: ViewModifier {
    let edge: VerticalEdge
    let color: Color
    let scrim: Bool
    let bar: Bar

    private var edges: Edge.Set { edge == .top ? .top : .bottom }

    @ViewBuilder
    func body(content: Content) -> some View {
        if #available(iOS 26.0, *) {
            content
                .safeAreaBar(edge: edge) { system }
                .scrollEdgeEffectStyle(.soft, for: edges)
        } else {
            content.safeAreaInset(edge: edge, spacing: 0) { faded }
        }
    }

    @ViewBuilder
    private var system: some View {
        if scrim { faded } else { bar }
    }

    /// The bar over `color`, solid at the screen's edge and clear at the
    /// bar's inner one.
    private var faded: some View {
        bar.background(
            LinearGradient(
                stops: [
                    .init(color: color, location: 0),
                    .init(color: color, location: 0.7),
                    .init(color: color.opacity(0), location: 1),
                ],
                startPoint: edge == .top ? .top : .bottom,
                endPoint: edge == .top ? .bottom : .top
            )
            .ignoresSafeArea(edges: edges)
        )
    }
}

/// Where the system lays a bar along the top or bottom of the screen,
/// which is not always the safe area's edge: an unfolded Duo, its status
/// bar at the side, has no safe area at the top, and a bar there belongs
/// below the rounded corners; at the bottom of a phone a bar sits partly
/// over the home indicator, as the system's tab bar does. iOS 27.1 says
/// where (`UIView.LayoutRegion.bar`); from iOS 26 the tab bar's place
/// stands in at the bottom. Measured on the window, from its safe area's
/// edges (a bottom bar's room below it is negative): the view backing a
/// SwiftUI one can be read mid-layout, somewhere it never shows, and is
/// given no safe area of its own.
private struct BarRoom: UIViewRepresentable {
    let edge: VerticalEdge
    let height: CGFloat
    @Binding var room: EdgeInsets

    func makeUIView(context: Context) -> Reader {
        let reader = Reader()
        reader.isUserInteractionEnabled = false
        return reader
    }

    func updateUIView(_ reader: Reader, context: Context) {
        let room = $room
        reader.found = { if room.wrappedValue != $0 { room.wrappedValue = $0 } }
        reader.edge = edge
        reader.height = height
        reader.read()
    }

    /// Reads the room again whenever the screen may have changed under
    /// it, and tells only a change, after the pass.
    final class Reader: UIView {
        var edge = VerticalEdge.top
        var height: CGFloat = 0
        var found: ((EdgeInsets) -> Void)?
        private var told: EdgeInsets?

        override func didMoveToWindow() {
            super.didMoveToWindow()
            read()
        }

        override func layoutSubviews() {
            super.layoutSubviews()
            read()
        }

        func read() {
            guard let window, height > 0 else { return }
            let rtl = window.effectiveUserInterfaceLayoutDirection == .rightToLeft
            let insets = window.safeAreaInsets
            let safe = NSDirectionalEdgeInsets(
                top: insets.top,
                leading: rtl ? insets.right : insets.left,
                bottom: insets.bottom,
                trailing: rtl ? insets.left : insets.right
            )
            var room = EdgeInsets()
            if #available(iOS 27.1, *) {
                let bar = window.directionalEdgeInsets(
                    for: .bar(onEdge: edge == .top ? NSDirectionalRectEdge.top : .bottom, extent: height)
                )
                room = EdgeInsets(
                    top: edge == .top ? bar.top - safe.top : 0,
                    leading: bar.leading - safe.leading,
                    bottom: edge == .bottom ? bar.bottom - safe.bottom : 0,
                    trailing: bar.trailing - safe.trailing
                )
            } else if #available(iOS 26.0, *), edge == .bottom, safe.bottom >= 20 {
                // The tab bar's middle is 52 points above the edge of a
                // phone with a home indicator.
                room.bottom = min(0, 52 - height / 2 - safe.bottom)
            }
            guard room != told else { return }
            told = room
            let found = self.found
            DispatchQueue.main.async { found?(room) }
        }
    }
}
