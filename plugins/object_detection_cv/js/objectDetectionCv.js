// SPDX-License-Identifier: GPL-2.0-or-later
// @ts-check

import {
	uniqueID,
	normalize,
	denormalize,
	globals,
	htmlToElem,
} from "./libs/common.js";
import {
	newForm,
	newNumberField,
	newModalField,
	newRawSelectField,
	fieldTemplate,
} from "./components/form.js";
import { newStreamer } from "./components/streamer.js";
import { newModal } from "./components/modal.js";
import { newPolygonEditor } from "./components/polygonEditor.js";

/**
 * @template T
 * @typedef {import("./components/form.js").Field<T>} Field
 */

/** @param {() => string} getMonitorId */
export function objectDetectionCv(getMonitorId) {
	const { monitorsInfo } = globals();

	/** @param {string} monitorID */
	const hasSubStream = (monitorID) => {
		if (monitorsInfo[monitorID] && monitorsInfo[monitorID].hasSubStream) {
			return monitorsInfo[monitorID].hasSubStream;
		}
		return false;
	};

	const fields = {
		enable: fieldTemplate.toggle("Enable CV object detection", false),
		feedRate: newNumberField(
			{ min: 0, step: 0.1 },
			"Feed rate (fps)",
			"",
			5,
			"Frames per second to analyze. The sub stream is much faster to process",
		),
		duration: fieldTemplate.integer(
			"Trigger duration (sec)",
			"",
			120,
			"The number of seconds the recorder will be active for after an object is detected",
		),
		useSubStream: fieldTemplate.toggle(
			"Use sub stream",
			true,
			"If sub stream should be used instead of the main stream",
		),
		scale: fieldTemplate.integer(
			"Analysis scale",
			"",
			1,
			"Downscale factor 1-8 for the analyzed luma plane. 2 halves the resolution and CPU cost",
		),
		mask: mask(hasSubStream, getMonitorId),
		background: group({
			samples: fieldTemplate.integer(
				"Samples",
				"",
				8,
				"Number of background samples per pixel",
			),
			threshold: fieldTemplate.integer(
				"Threshold",
				"",
				20,
				"Luma distance for a sample to count as a match",
			),
			minMatch: fieldTemplate.integer(
				"Min match",
				"",
				2,
				"Matching samples required for a pixel to be background",
			),
			updateRate: fieldTemplate.integer(
				"Update rate",
				"",
				16,
				"1 in N chance to update a pixel model each frame",
			),
		}),
		blob: group({
			minArea: newNumberField(
				{ min: 0, step: 0.1 },
				"Min area (%)",
				"",
				0.2,
				"Minimum blob area as a percent of the frame",
			),
			maxArea: newNumberField(
				{ min: 0, step: 0.1 },
				"Max area (%)",
				"",
				50,
				"Maximum blob area as a percent of the frame",
			),
			minAspect: newNumberField(
				{ min: 0, step: 0.1 },
				"Min aspect",
				"",
				0.1,
				"Minimum blob width / height ratio",
			),
			maxAspect: newNumberField(
				{ min: 0, step: 0.1 },
				"Max aspect",
				"",
				10,
				"Maximum blob width / height ratio",
			),
			open: fieldTemplate.toggle("Open", true, "Remove single-pixel speckle"),
			close: fieldTemplate.toggle("Close", true, "Fill small holes in blobs"),
		}),
		tracker: group({
			minHits: fieldTemplate.integer(
				"Min hits",
				"",
				2,
				"Consecutive frames before a track is confirmed",
			),
			maxAge: fieldTemplate.integer(
				"Max age (frames)",
				"",
				5,
				"Missed frames before a track is dropped",
			),
			iou: newNumberField(
				{ min: 0, step: 0.05 },
				"Min IoU",
				"",
				0.3,
				"Minimum overlap to associate a detection with a track",
			),
			cooldown: newNumberField(
				{ min: 0, step: 1 },
				"Cooldown (sec)",
				"",
				5,
				"Seconds before the same track may trigger again. Derived from the feed rate; a feed rate of 0 disables it",
			),
		}),
	};

	const form = newForm(fields);
	const modal = newModal("CV object detection", [form.elem()]);

	let value;

	let isRendered = false;
	const render = () => {
		if (isRendered) {
			return;
		}
		elem.append(modal.elem);

		isRendered = true;
		value = value === undefined ? {} : value;
		form.set(value);
	};

	const open = () => {
		render();
		modal.open();
	};

	const elem = newModalField("CV object detection", open);

	return {
		elems: [elem],
		value() {
			if (isRendered) {
				value = {};
				form.get(value);
			}
			return value;
		},
		set(input) {
			value = input === undefined ? {} : input;
			if (isRendered) {
				form.set(value);
			}
		},
		validate() {
			if (!isRendered) {
				return;
			}
			const err = form.validate();
			if (err !== undefined) {
				return `CV object detection: ${err}`;
			}
		},
		// @ts-ignore
		openTesting() {
			open();
		},
	};
}

/**
 * Groups fields into a nested object, rendered inline in the parent form.
 * @param {{[x: string]: Field<any>}} fields
 * @returns {Field<any>}
 */
function group(fields) {
	const form = newForm(fields);

	/** @type {Element[]} */
	const elems = [];
	for (const field of Object.values(fields)) {
		elems.push(...field.elems);
	}

	let value = {};

	return {
		elems,
		value() {
			value = {};
			form.get(value);
			return value;
		},
		set(input) {
			value = input === undefined ? {} : input;
			form.set(value);
		},
		validate() {
			return form.validate();
		},
	};
}

/** @typedef { import("./components/feed.js").Feed } Feed */

/**
 * @typedef {Object} Mask
 * @property {boolean} enable
 * @property {[number,number][]} area
 */

function newMaskOptions() {
	/** @type  {HTMLButtonElement} */
	// @ts-ignore
	const $x1 = htmlToElem(/* HTML */ `
		<button
			class="pl-2 pr-1 text-1.4 text-color bg-color2 hover:bg-color1"
			style="
				border-top-left-radius: var(--radius-xl);
				border-bottom-left-radius: var(--radius-xl);
			"
		>
			1x
		</button>
	`);
	/** @type {HTMLButtonElement} */
	// @ts-ignore
	const $x4 = htmlToElem(/* HTML */ `
		<button
			class="px-1 text-1.4 text-color bg-color2 hover:bg-color1 object_detection_cv_mask-step-size-selected"
		>
			4x
		</button>
	`);
	/** @type {HTMLButtonElement} */
	// @ts-ignore
	const $x10 = htmlToElem(/* HTML */ `
		<button class="px-1 text-1.4 text-color bg-color2 hover:bg-color1">10x</button>
	`);
	/** @type {HTMLButtonElement} */
	// @ts-ignore
	const $x20 = htmlToElem(/* HTML */ `
		<button
			class="pl-1 pr-2 text-1.4 text-color bg-color2 hover:bg-color1"
			style="
				border-top-right-radius: var(--radius-xl);
				border-bottom-right-radius: var(--radius-xl);
			"
		>
			20x
		</button>
	`);
	/** @type {HTMLInputElement} */
	// @ts-ignore
	const $x = htmlToElem(/* HTML */ `
		<input
			class="mr-1 text-center text-1.4"
			style="width: calc(var(--scale) * 3.5rem);"
			type="number"
			min="0"
			max="100"
		/>
	`);
	/** @type {HTMLInputElement} */
	// @ts-ignore
	const $y = htmlToElem(/* HTML */ `
		<input
			class="text-center text-1.4"
			style="width: calc(var(--scale) * 3.5rem);"
			type="number"
			min="0"
			max="100"
		/>
	`);
	const elem = htmlToElem(
		/* HTML */ `
			<li
				class="flex items-center p-2 border-b-2 border-color1"
				style="flex-wrap: wrap; justify-content: space-between;"
			></li>
		`,
		[
			htmlToElem(
				//
				`<div class="flex"></div>`,
				[$x1, $x4, $x10, $x20],
			),
			htmlToElem(
				//
				`<div class="flex"></div>`,
				[$x, $y],
			),
		],
	);
	return { elem, $x1, $x4, $x10, $x20, $x, $y };
}

/**
 * @param {(monitorID: string) => boolean} hasSubStream
 * @param {() => string} getMonitorId
 * @returns {Field<Mask>}
 */
function mask(hasSubStream, getMonitorId) {
	/** @type {Mask} */
	let value;

	let editor;

	const $feed = htmlToElem(
		`<div class="flex w-full" style="min-width: 0; background: white;"></div>`,
	);

	const enable = newRawSelectField("Enable", ["true", "false"]);
	const modal = newModal("Mask");

	const renderModal = () => {
		const maskOptions = newMaskOptions();
		const $overlay = htmlToElem(/* HTML */ `
			<svg
				class="absolute w-full h-full"
				style="
					top: 0;
					z-index: 1;
					user-select: none;
					overflow: visible;
				"
				viewBox="0 0 100 100"
				preserveAspectRatio="none"
			></svg>
		`);
		const elems = [
			enable.elem,
			htmlToElem(
				//
				`<li class="flex flex-col items-center px-2"></li>`,
				[
					htmlToElem(
						`<label class="grow mr-auto text-1.5 text-color">Preview</label>`,
					),
					htmlToElem(
						//
						`<div class="relative"></div>`,
						[$feed, $overlay],
					),
				],
			),
			maskOptions.elem,
		];

		modal.$content.replaceChildren(...elems);

		enable.$input.value = String(value.enable);
		enable.$input.addEventListener("change", () => {
			value.enable = enable.$input.value === "true";
		});

		editor = newPolygonEditor($overlay, {
			opacity: 0.4,
			stepSize: 4,
			onChange: (_, x, y) => {
				maskOptions.$x.value = String(x);
				maskOptions.$y.value = String(y);
			},
		});
		editor.set(value.area);

		/** @param {number} v */
		const setStepSize = (v) => {
			const selectedClass = "object_detection_cv_mask-step-size-selected";
			maskOptions.$x1.classList.remove(selectedClass);
			maskOptions.$x4.classList.remove(selectedClass);
			maskOptions.$x10.classList.remove(selectedClass);
			maskOptions.$x20.classList.remove(selectedClass);

			switch (v) {
				case 1: {
					maskOptions.$x1.classList.add(selectedClass);
					break;
				}
				case 4: {
					maskOptions.$x4.classList.add(selectedClass);
					break;
				}
				case 10: {
					maskOptions.$x10.classList.add(selectedClass);
					break;
				}
				case 20: {
					maskOptions.$x20.classList.add(selectedClass);
					break;
				}
				default: {
					throw new Error(`invalid step size: ${v}`);
				}
			}

			editor.setStepSize(v);
		};

		maskOptions.$x1.onclick = () => {
			setStepSize(1);
		};
		maskOptions.$x4.onclick = () => {
			setStepSize(4);
		};
		maskOptions.$x10.onclick = () => {
			setStepSize(10);
		};
		maskOptions.$x20.onclick = () => {
			setStepSize(20);
		};

		const $x = maskOptions.$x;
		const $y = maskOptions.$y;
		$x.onchange = () => {
			$x.value = String(Math.min(100, Math.max(0, Number($x.value))));
			editor.setIndex(editor.selected(), Number($x.value), Number($y.value));
		};
		$y.onchange = () => {
			$y.value = String(Math.min(100, Math.max(0, Number($y.value))));
			editor.setIndex(editor.selected(), Number($x.value), Number($y.value));
		};
	};

	/** @return {Mask} */
	const initialValue = () => {
		return {
			area: [
				[30, 20],
				[70, 20],
				[70, 80],
				[30, 80],
			],
			enable: false,
		};
	};

	/** @type {Feed} */
	let feed;

	let rendered = false;
	const elem = newModalField(
		"Mask",
		() => {
			if (feed !== undefined) {
				feed.destroy();
			}
			const monitor = {
				id: getMonitorId(),
				audioEnabled: "false",
				hasSubStream: hasSubStream(getMonitorId()),
			};
			feed = newStreamer(monitor, true, globals().flags.streamer);

			if (!rendered) {
				renderModal();
				rendered = true;
			}
			modal.onClose(() => {
				feed.destroy();
			});
			$feed.replaceChildren(feed.elem);

			modal.open();
		},
		"Mask off areas you want the detector to ignore. The dark area will be ignored.",
	);

	return {
		elems: [elem, modal.elem],
		value() {
			if (rendered) {
				return normalizeMask({
					enable: enable.$input.value === "true",
					area: editor.value(),
				});
			}
			return normalizeMask(value);
		},
		set(input) {
			// @ts-ignore
			value = input === undefined ? initialValue() : denormalizeMask(input);
			if (rendered) {
				enable.$input.value = String(value.enable);
				editor.set(value.area);
			}
		},
	};
}

/**
 * @param {Mask} mask
 * @returns {Mask}
 */
function denormalizeMask(mask) {
	return {
		enable: mask.enable,
		area: mask.area.map(([x, y]) => {
			return [denormalize(x, 100), denormalize(y, 100)];
		}),
	};
}

/**
 * @param {Mask} mask
 * @returns {Mask}
 */
function normalizeMask(mask) {
	return {
		enable: mask.enable,
		area: mask.area.map(([x, y]) => {
			return [normalize(x, 100), normalize(y, 100)];
		}),
	};
}

// CSS.
const $style = document.createElement("style");
$style.innerHTML = /* CSS */ `
	/* Mask. */
	.object_detection_cv_mask-step-size-selected {
		background: var(--color1) !important;
	}
`;

document.querySelector("head").append($style);
