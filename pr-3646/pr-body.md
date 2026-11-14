No issue

Follows #2739.

## What it is for

Creators can now take an animation made in Avatar Studio back to Blender (or any other BVH tool): **Export .bvh** writes the clip on the avatar's skeleton, next to the existing Save .anim. The timeline can also pick several keys at once, so they can be moved, copied or deleted together instead of one by one, and the animation editor gains several quality-of-life fixes from a day of hands-on testing: several joints edited together, finer handle drags, a steady camera, an R key to switch Move and Rotate, and a cleaner panel.

## Legacy Viewer origin

No for the editor features (the Legacy Viewer has no animation editor). The BVH writer is the inverse of the existing import, which follows `LLBVHLoader::loadBVHFile` / `makeTranslation` (indra/llcharacter/llbvhloader.cpp): same Y-up inches to Z-up metres basis, frame 0 as the rest reference the pelvis is measured from, Zrotation Xrotation Yrotation channel order.

## What changes

- **Export:** `aurora_assets::import_animation::encode_bvh` writes every bone of the rest skeleton (no collision volumes) with its rest OFFSETs, a rest reference frame, then one frame per `1 / fps` up to the clip's duration. The pelvis carries position channels over its rest offset (the hips keep their height in Blender), and so does any bone the clip moves. Angles are filtered from frame to frame (each within half a turn of the last, and the other Euler set of the same rotation when it is closer), so a tool that interpolates between frames never spins a bone the long way round.
- **Interface, editor panel:** a new **Export .bvh** button between Save .anim and Prepare upload; it follows the same "loop to last key" range as Save .anim. Its tooltip says what a BVH cannot hold (priorities, loop, ease, hand pose).
- **Interface, timeline:** keys can be picked several at a time. A click picks one key, Ctrl (Cmd) + click toggles it, Shift + click adds it, Shift or Ctrl + drag anywhere over the lanes (on a key, between rows or below them) draws a box that picks the keys inside it, Ctrl+A picks every shown key and Esc drops them. A plain click on an empty spot drops them too; a plain drag on the lanes still scrubs as before. Picked keys are drawn in teal.
- **Editing:** dragging one picked key moves them all by the same number of frames (the playhead stays where it is), with a ghost of each destination while dragging; the move stops at frame 0 and grows the range past the end, and keys moving onto each other's frames do not replace one another. Delete (and the trash button) removes every picked key. With several keys picked, Ctrl+C copies the keys themselves and Ctrl+V pastes them at the playhead with their spacing, and the pasted keys are picked; with one key picked (a plain click), Ctrl+C still copies the frame's pose as before. Each of these is one undo step.
- **Quality of life:** the timeline status line says how many keys are picked and what can be done with them; the timeline File menu also has Save .anim and Export .bvh, so the timeline alone covers the whole loop. Shortcuts keep the existing rule: Ctrl+A, Esc and Delete only take their key when they act.
- **Timeline, after testing:** the box no longer has to start on an empty spot of a lane, and a box started between or under the lanes no longer drags the timeline window. A right-click over the lanes drops the picked keys (it never deletes them).
- **Several joints at once (animation editor):** Shift or Ctrl + click on a joint handle, a bone, a picker dot or a Tracks row adds the joint to the selection; a plain click picks one again. The joints added follow the selected one: a turn turns each of them by the same local rotation, a move moves each by the same offset, and all of them are keyed together (their mirrors too when Symmetry is on). The added joints are ringed in teal, and the Bone page shows how many follow with an "Only this one" button. In the pose editor, Ctrl+click now adds a joint on its marker and shaft as well as on its row (Shift+click already did).
- **Finer handle drags:** holding Shift during a handle drag moves and turns the joint ten times finer, in the pose and the animation editors; pressing or releasing Shift mid-drag rebases the drag so the joint does not jump.
- **Camera and tools:** a press on a joint's handle or bone no longer also turns Avatar Studio's camera. A vertical left drag on the stage tilts the camera up or down, to 80 degrees at most and never under the stage floor, where the stage's water fog would hide the avatar; Reset returns to the default view. **R** switches the gizmo between Move and Rotate; the tool stays from joint to joint, and Shift no longer turns Rotate into Move in the animation editor.
- **Steady fields:** an edited joint's position and rotation fields no longer flicker while nothing changes (they were re-read from the shown pose every frame).
- **Animation editor panel:** the Bone page gets the interface's section header and icon buttons (Reset this bone, Reset pose) and a short hint line for the shortcuts; the footer's key, save, export and upload buttons all carry a filled Tabler icon the viewer embeds and share one width. The Drafts menu lists every draft at a fixed place with the open one ticked, instead of reshuffling on each switch. The bone picker is drawn wider and taller so its dots are easier to hit.
- **Translations:** new and changed keys in English, French and Japanese.

## Screenshots

Demo mode, 2026-11-14.

Animation timeline with the whole clip keyed:

![timeline](https://raw.githubusercontent.com/Nowalski/Aurora-Viewer/captures/pr-3646/timeline.png)

Animation editor: Bone page, wider bone picker, icon buttons of equal width:

![animation editor](https://raw.githubusercontent.com/Nowalski/Aurora-Viewer/captures/pr-3646/animation-editor.png)

Timeline File menu with Save .anim and Export .bvh:

![file menu](https://raw.githubusercontent.com/Nowalski/Aurora-Viewer/captures/pr-3646/timeline-file-menu.png)

Picking keys on the timeline:

![box select](https://raw.githubusercontent.com/Nowalski/Aurora-Viewer/captures/pr-3646/timeline-box-select.gif)

Fine handle drag (Shift) on a hand joint:

![fine move](https://raw.githubusercontent.com/Nowalski/Aurora-Viewer/captures/pr-3646/fine-move.gif)

## Tests

- [x] New unit tests: a BVH exported from a clip imports back (`parse_bvh_for_edit`) to the same rotations and pelvis positions; its hierarchy is the rest skeleton without collision volumes, with a rest frame 0; an unknown joint is refused; angles never jump a full turn between frames. Group moves keep spacing, stop at frame 0, grow the range and do not replace each other; deleting picked keys keeps a draft's hidden rest keys; pick rules (click, Ctrl, Shift, drag); box hit-testing; an egui test that drags one of two picked keys and checks both move in one undo step; an egui test where Shift + drag from a key boxes keys without moving it; copy and paste of picked keys keep their spacing and values; Delete, Ctrl+A and Esc through the shortcut handler.
- [x] `cargo fmt --check`, clippy `-D warnings` and `cargo test --workspace` on Linux: the new code is clean and every test passes except the already known `chat_log::import::tests::a_failure_leaves_the_file_as_it_was`, which fails only when the tests run as root.
- [ ] `scripts/check.ps1` on Windows.
- [x] More unit tests for the later rounds: a box dragged from under the lanes does not move the window; a right-click over the lanes drops the picked keys; Ctrl or Shift + click adds a joint in the animation editor and a plain click resets; joints selected together turn with the selected one and are keyed with it; the animation editor keeps its tool and Shift does not turn Rotate into Move; an edited joint's fields hold still while the shown pose only jitters; Shift during a handle drag is ten times finer without a jump, in the pose and animation editors only; the Drafts list shows every draft in its place while switching; the animation editor's buttons only use icons the viewer embeds.
- [x] Validated by the human in demo mode on 2026-11-14, over five test rounds: a clip exported as .bvh, opened in Blender and imported back, key picking (click, Ctrl, Shift, box, Ctrl+A, Esc, right-click), moving, copying and deleting picked keys, several joints edited together, Shift fine drags, R, the camera staying still, the Drafts menu and the panel's look.

## Limits and follow-up

- A BVH has no place for priorities, loop points, ease, hand pose or expression: they stay in the .anim only.
- A bone keyed at its rest rotation for the whole clip is not distinguishable from an unkeyed one in a BVH, so it does not come back as a track on re-import (its motion is unchanged).
