// SPDX-License-Identifier: GPL-2.0-or-later
// @ts-check

import { denormalize, relativePathname, htmlToElem, sleep } from "./libs/common.js";

/**
 * @typedef {import("./components/feed.js").Feed} Feed
 */

const POLL_INTERVAL_MS = 500;

/**
 * Wraps a live feed with an overlay showing the current detections.
 *
 * Polls the object detection plugin for the current detections and renders
 * them as rectangles on top of the video.
 *
 * @param {Feed} feed
 * @param {{id: string}} monitor
 * @returns {Feed}
 */
export function newLiveDetections(feed, monitor) {
	const monitorId = monitor["id"];

	/** @type {HTMLVideoElement} */
	// @ts-ignore
	const $video = feed.elem.querySelector("video");
	if (!$video) {
		return feed;
	}
	const $host = $video.parentElement;
	if (!$host) {
		return feed;
	}

	/** @type {SVGElement} */
	// @ts-ignore
	const $svg = htmlToElem(/* HTML */ `
		<svg
			class="js-live-detections absolute"
			style="
				stroke: var(--color-red);
				fill-opacity: 0;
				stroke-width: 1.5px;
				pointer-events: none;
				z-index: 3;
			"
			viewBox="0 0 100 100"
			preserveAspectRatio="none"
		></svg>
	`);
	$host.append($svg);

	// Text labels are plain HTML: inside the stretched SVG the font would
	// scale with the feed and get distorted.
	/** @type {HTMLDivElement} */
	// @ts-ignore
	const $labels = htmlToElem(/* HTML */ `
		<div
			class="js-live-detections-labels absolute"
			style="
				pointer-events: none;
				z-index: 3;
				overflow: hidden;
			"
		></div>
	`);
	$host.append($labels);

	// Match the video content box; the video letterboxes inside the container
	// (object-fit: contain). The labels container must follow the same box,
	// or the labels would drift onto the letterbox bars.
	const position = () => {
		const cw = $host.clientWidth;
		const ch = $host.clientHeight;
		const vw = $video.videoWidth;
		const vh = $video.videoHeight;
		if (cw === 0 || ch === 0 || vw === 0 || vh === 0) {
			return;
		}
		const scale = Math.min(cw / vw, ch / vh);
		const w = vw * scale;
		const h = vh * scale;
		for (const elem of [$svg, $labels]) {
			elem.style.left = `${(cw - w) / 2}px`;
			elem.style.top = `${(ch - h) / 2}px`;
			elem.style.width = `${w}px`;
			elem.style.height = `${h}px`;
		}
	};
	$video.addEventListener("loadedmetadata", position);
	// eslint-disable-next-line compat/compat
	new ResizeObserver(position).observe($host);
	position();

	/** @param {any[]} detections */
	const render = (detections) => {
		let rects = "";
		let labels = "";
		for (const d of detections) {
			if (!d.region || !d.region.rectangle) {
				continue;
			}
			const r = d.region.rectangle;
			const x = denormalize(r.x, 100);
			const y = denormalize(r.y, 100);
			const width = denormalize(r.width, 100);
			const height = denormalize(r.height, 100);

			// The stroke is screen-space via non-scaling-stroke, so the
			// border stays thin on any feed size.
			rects += `
				<rect
					x="${x}"
					width="${width}"
					y="${y}"
					height="${height}"
					vector-effect="non-scaling-stroke"
				/>
			`;

			// Label above the box, or below it when the box touches the top.
			const above = y > 8;
			const labelY = above ? y : y + height;
			const shift = above ? "translateY(-100%)" : "translateY(0.3rem)";
			labels += `
				<div
					class="js-live-detections-label"
					style="left: ${x}%; top: ${labelY}%; transform: ${shift};"
				>${d.label} ${Math.round(d.score)}%</div>
			`;
		}
		$svg.innerHTML = rects;
		$labels.innerHTML = labels;
	};

	const abort = new AbortController();
	let stopped = false;

	(async () => {
		const url = `${relativePathname("api/monitor")}/${monitorId}/object-detection-cv/detections`;
		while (!stopped) {
			try {
				const response = await fetch(url, { signal: abort.signal });
				if (response.status === 404) {
					// Monitor or route is gone.
					return;
				}
				if (response.status === 200) {
					render(await response.json());
				}
			} catch (error) {
				if (!(error instanceof DOMException && error.name === "AbortError")) {
					console.error(error);
				}
			}
			if (await sleep(abort.signal, POLL_INTERVAL_MS)) {
				return;
			}
		}
	})();

	return {
		elem: feed.elem,
		enableDebugging() {
			feed.enableDebugging();
		},
		destroy() {
			stopped = true;
			abort.abort("cancelled");
			$svg.remove();
			$labels.remove();
			feed.destroy();
		},
	};
}

// CSS.
const $style = document.createElement("style");
$style.innerHTML = /* CSS */ `
	.js-live-detections-label {
		position: absolute;
		color: var(--color-red);
		font-size: calc(var(--scale) * 0.9rem);
		line-height: 1.1;
		white-space: nowrap;
		text-shadow: 0 0 3px rgba(0, 0, 0, 0.85);
	}
`;

document.querySelector("head").append($style);
