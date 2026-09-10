// The lucide icons the desktop draws its chrome with, inlined as the wasm
// inlined them (thinkterm-web/src/icons.rs): the file verbatim, taking its
// colour from CSS `color` and its size from CSS.

import archive from '../../../third_party/lucide/icons/archive.svg?raw';
import archiveRestore from '../../../third_party/lucide/icons/archive-restore.svg?raw';
import bell from '../../../third_party/lucide/icons/bell.svg?raw';
import info from '../../../third_party/lucide/icons/info.svg?raw';
import minus from '../../../third_party/lucide/icons/minus.svg?raw';
import notebookTabs from '../../../third_party/lucide/icons/notebook-tabs.svg?raw';
import palette from '../../../third_party/lucide/icons/palette.svg?raw';
import bot from '../../../third_party/lucide/icons/bot.svg?raw';
import check from '../../../third_party/lucide/icons/check.svg?raw';
import chevronDown from '../../../third_party/lucide/icons/chevron-down.svg?raw';
import chevronRight from '../../../third_party/lucide/icons/chevron-right.svg?raw';
import circleAlert from '../../../third_party/lucide/icons/circle-alert.svg?raw';
import circleCheck from '../../../third_party/lucide/icons/circle-check.svg?raw';
import circlePlus from '../../../third_party/lucide/icons/circle-plus.svg?raw';
import codeXml from '../../../third_party/lucide/icons/code-xml.svg?raw';
import clipboardPaste from '../../../third_party/lucide/icons/clipboard-paste.svg?raw';
import copy from '../../../third_party/lucide/icons/copy.svg?raw';
import ellipsis from '../../../third_party/lucide/icons/ellipsis.svg?raw';
import eye from '../../../third_party/lucide/icons/eye.svg?raw';
import folderTree from '../../../third_party/lucide/icons/folder-tree.svg?raw';
import folder from '../../../third_party/lucide/icons/folder.svg?raw';
import folderOpen from '../../../third_party/lucide/icons/folder-open.svg?raw';
import folderPlus from '../../../third_party/lucide/icons/folder-plus.svg?raw';
import globe from '../../../third_party/lucide/icons/globe.svg?raw';
import grid2x2 from '../../../third_party/lucide/icons/grid-2x2.svg?raw';
import house from '../../../third_party/lucide/icons/house.svg?raw';
import languages from '../../../third_party/lucide/icons/languages.svg?raw';
import layers from '../../../third_party/lucide/icons/layers.svg?raw';
import loaderCircle from '../../../third_party/lucide/icons/loader-circle.svg?raw';
import mail from '../../../third_party/lucide/icons/mail.svg?raw';
import maximize2 from '../../../third_party/lucide/icons/maximize-2.svg?raw';
import minimize2 from '../../../third_party/lucide/icons/minimize-2.svg?raw';
import panelLeft from '../../../third_party/lucide/icons/panel-left.svg?raw';
import panelRight from '../../../third_party/lucide/icons/panel-right.svg?raw';
import pencil from '../../../third_party/lucide/icons/pencil.svg?raw';
import pin from '../../../third_party/lucide/icons/pin.svg?raw';
import pinOff from '../../../third_party/lucide/icons/pin-off.svg?raw';
import plus from '../../../third_party/lucide/icons/plus.svg?raw';
import rotateCcw from '../../../third_party/lucide/icons/rotate-ccw.svg?raw';
import search from '../../../third_party/lucide/icons/search.svg?raw';
import settings from '../../../third_party/lucide/icons/settings.svg?raw';
import slidersHorizontal from '../../../third_party/lucide/icons/sliders-horizontal.svg?raw';
import squareSplitHorizontal from '../../../third_party/lucide/icons/square-split-horizontal.svg?raw';
import squareSplitVertical from '../../../third_party/lucide/icons/square-split-vertical.svg?raw';
import squareTerminal from '../../../third_party/lucide/icons/square-terminal.svg?raw';
import trash2 from '../../../third_party/lucide/icons/trash-2.svg?raw';
import x from '../../../third_party/lucide/icons/x.svg?raw';
import zoomIn from '../../../third_party/lucide/icons/zoom-in.svg?raw';
import zoomOut from '../../../third_party/lucide/icons/zoom-out.svg?raw';
// The two brand marks the Agents panel draws (simple-icons): a single
// fill-less path, so they take their colour from CSS `fill: currentColor`
// rather than from `stroke` as the lucide icons above do.
import brandClaude from '../../../third_party/simple-icons/icons/claude.svg?raw';
import brandCopilot from '../../../third_party/simple-icons/icons/githubcopilot.svg?raw';

export {
  archive,
  archiveRestore,
  bell,
  bot,
  brandClaude,
  brandCopilot,
  check,
  chevronDown,
  chevronRight,
  circleAlert,
  circleCheck,
  circlePlus,
  clipboardPaste,
  codeXml,
  copy,
  ellipsis,
  eye,
  folder,
  folderOpen,
  folderTree,
  globe,
  grid2x2,
  folderPlus,
  house,
  info,
  languages,
  layers,
  loaderCircle,
  mail,
  maximize2,
  minimize2,
  minus,
  notebookTabs,
  palette,
  panelLeft,
  panelRight,
  pencil,
  pin,
  pinOff,
  plus,
  rotateCcw,
  search,
  settings,
  slidersHorizontal,
  squareSplitHorizontal,
  squareSplitVertical,
  squareTerminal,
  trash2,
  x,
  zoomIn,
  zoomOut,
};

// The menus name their icon the way lucide does (thinkterm-web/src/menu.rs),
// so a row's `icon` is looked up here rather than matched on in the markup.
const BY_NAME: Record<string, string> = {
  palette,
  minus,
  info,
  bell,
  archive,
  'archive-restore': archiveRestore,
  bot,
  check,
  'chevron-down': chevronDown,
  'chevron-right': chevronRight,
  'circle-alert': circleAlert,
  'circle-check': circleCheck,
  'circle-plus': circlePlus,
  'clipboard-paste': clipboardPaste,
  copy,
  ellipsis,
  eye,
  folder,
  'folder-open': folderOpen,
  'folder-plus': folderPlus,
  'folder-tree': folderTree,
  'notebook-tabs': notebookTabs,
  'code-xml': codeXml,
  globe,
  'grid-2x2': grid2x2,
  house,
  languages,
  layers,
  'loader-circle': loaderCircle,
  mail,
  'maximize-2': maximize2,
  'minimize-2': minimize2,
  'panel-left': panelLeft,
  'panel-right': panelRight,
  pencil,
  pin,
  'pin-off': pinOff,
  plus,
  'rotate-ccw': rotateCcw,
  search,
  settings,
  'sliders-horizontal': slidersHorizontal,
  'square-split-horizontal': squareSplitHorizontal,
  'square-split-vertical': squareSplitVertical,
  'square-terminal': squareTerminal,
  'trash-2': trash2,
  x,
  'zoom-in': zoomIn,
  'zoom-out': zoomOut,
};

/** A lucide icon by its name; nothing for a name the page has not inlined. */
export function iconByName(name: string): string | undefined {
  return BY_NAME[name];
}

// An agent row names its mark the way `agents.rs` does: a brand when the
// page has one, `bot` otherwise. Kept apart from `BY_NAME` above, which is
// lucide's names and nothing else.
const AGENT_ICONS: Record<string, string> = {
  'brand-claude': brandClaude,
  'brand-copilot': brandCopilot,
  bot,
};

/** An agent row's icon by its name; the bot for one the page has not inlined. */
export function agentIcon(name: string): string {
  return AGENT_ICONS[name] ?? bot;
}
