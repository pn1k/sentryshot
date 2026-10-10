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
		enable: fieldTemplate.toggle("Включить детекцию объектов (CV)", false),
		feedRate: newNumberField(
			{ min: 0, step: 0.1 },
			"Частота анализа (кадр/с)",
			"",
			5,
			"Сколько кадров в секунду анализировать. Суб-поток обрабатывать значительно дешевле",
		),
		duration: fieldTemplate.integer(
			"Длительность триггера (сек)",
			"",
			120,
			"Сколько секунд запись остаётся активной после обнаружения объекта",
		),
		useSubStream: fieldTemplate.toggle(
			"Использовать суб-поток",
			true,
			"Анализировать суб-поток вместо основного потока",
		),
		scale: fieldTemplate.integer(
			"Масштаб анализа",
			"",
			1,
			"Коэффициент уменьшения 1-8 для анализируемой яркостной плоскости. 2 вдвое снижает разрешение и нагрузку на CPU",
		),
		mask: mask(hasSubStream, getMonitorId),
		background: group({
			samples: fieldTemplate.integer(
				"Сэмплы",
				"",
				8,
				"Количество фоновых сэмплов на пиксель",
			),
			threshold: fieldTemplate.integer(
				"Порог",
				"",
				20,
				"Расстояние по яркости, при котором сэмпл считается совпадением",
			),
			minMatch: fieldTemplate.integer(
				"Мин. совпадений",
				"",
				2,
				"Сколько сэмплов должны совпасть, чтобы пиксель считался фоном",
			),
			updateRate: fieldTemplate.integer(
				"Частота обновления",
				"",
				16,
				"Шанс 1 к N обновить модель пикселя в каждом кадре",
			),
		}),
		blob: group({
			minArea: newNumberField(
				{ min: 0, step: 0.1 },
				"Мин. площадь (%)",
				"",
				0.2,
				"Минимальная площадь блоба в процентах от кадра",
			),
			maxArea: newNumberField(
				{ min: 0, step: 0.1 },
				"Макс. площадь (%)",
				"",
				50,
				"Максимальная площадь блоба в процентах от кадра",
			),
			minAspect: newNumberField(
				{ min: 0, step: 0.1 },
				"Мин. пропорции",
				"",
				0.1,
				"Минимальное отношение ширины / высоты блоба",
			),
			maxAspect: newNumberField(
				{ min: 0, step: 0.1 },
				"Макс. пропорции",
				"",
				10,
				"Максимальное отношение ширины / высоты блоба",
			),
			open: fieldTemplate.toggle("Открытие", true, "Убирает однопиксельный шум"),
			close: fieldTemplate.toggle("Закрытие", true, "Заполняет мелкие дыры в блобах"),
		}),
		tracker: group({
			minHits: fieldTemplate.integer(
				"Мин. попаданий",
				"",
				2,
				"Подряд идущие кадры с объектом до подтверждения трека",
			),
			maxAge: fieldTemplate.integer(
				"Макс. возраст (кадры)",
				"",
				5,
				"Сколько кадров без объекта ждать до удаления трека",
			),
			iou: newNumberField(
				{ min: 0, step: 0.05 },
				"Мин. IoU",
				"",
				0.3,
				"Минимальное пересечение, чтобы связать детекцию с треком",
			),
			cooldown: newNumberField(
				{ min: 0, step: 1 },
				"Пауза (сек)",
				"",
				5,
				"Секунды до повторного срабатывания того же трека. Считается из частоты анализа; feedRate = 0 отключает паузу",
			),
		}),
	};

	const form = newForm(fields);
	const modal = newModal("Детекция объектов (CV)", [form.elem()]);

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

	const elem = newModalField("Детекция объектов (CV)", open);

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
				return `Детекция объектов (CV): ${err}`;
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

	const enable = newRawSelectField("Включена", ["true", "false"]);
	const modal = newModal("Маска");

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
					`<label class="grow mr-auto text-1.5 text-color">Предпросмотр</label>`,
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
		"Маска",
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
		"Скройте зоны, которые детектор должен игнорировать. Тёмная область будет игнорироваться.",
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
