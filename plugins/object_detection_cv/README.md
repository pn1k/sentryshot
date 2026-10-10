# object_detection_cv

Object detection plugin using classical computer vision: a ViBe-style
background model plus blob analysis and an IoU tracker. It is designed for
weak CPUs (works in real time on an Intel Celeron N4020 at 640x360) and needs
no neural network runtime, models or downloads.

Per-monitor settings live under the `objectDetectionCv` key of the monitor
config. All fields except `enable`, `feedRate`, `duration` and `useSubStream`
are optional.

| Field | Default | Description |
| --- | --- | --- |
| `enable` | `false` | Enable detection for the monitor |
| `feedRate` | `5` | Frames per second to analyze |
| `duration` | `120` | Seconds the recorder stays active after a trigger |
| `useSubStream` | `true` | Analyze the sub stream instead of the main stream |
| `scale` | `1` | Integer downscale factor for the analyzed luma plane (1-8). `2` halves resolution and CPU cost |
| `mask.enable` | `false` | Exclude pixels inside the mask polygon from detection; blobs are cut at the polygon boundary |
| `mask.area` | `[]` | Normalized polygon points (`1_000_000` = 100%) |
| `background.samples` | `8` | Background samples per pixel |
| `background.threshold` | `20` | Luma distance for a sample to count as a match |
| `background.minMatch` | `2` | Matching samples required for a pixel to be background |
| `background.updateRate` | `16` | 1 in N chance to update a background pixel model |
| `background.reinitFg` | `60` | Foreground percent above which the frame counts as a global change: no triggers are produced and the reinit streak grows |
| `background.reinitStreak` | `6` | Consecutive global-change frames before the model reinitializes |
| `blob.minArea` | `0.2` | Minimum blob area, percent of the frame |
| `blob.maxArea` | `50.0` | Maximum blob area, percent of the frame |
| `blob.minAspect` | `0.1` | Minimum blob width / height ratio |
| `blob.maxAspect` | `10.0` | Maximum blob width / height ratio |
| `blob.open` | `true` | 3x3 opening, removes single-pixel speckle |
| `blob.close` | `true` | 3x3 closing, fills small holes |
| `tracker.minHits` | `2` | Consecutive hit frames before a track is confirmed; a single miss drops an unconfirmed track |
| `tracker.maxAge` | `5` | Missed frames before a confirmed track is dropped |
| `tracker.iou` | `0.3` | Minimum `IoU` to associate a detection with a track |
| `tracker.cooldown` | `5` | Seconds before the same track may trigger again. Derived from the feed rate; a feed rate of `0` disables it |
| `tracker.staticAfter` | `60` | Seconds without significant movement after which a confirmed track is absorbed into the background (parked car, moved furniture). `0` disables absorption |

Notes:

- Only the luma (Y) plane is used; chroma is ignored, which matches how the
  other detectors in this project work and keeps the pipeline ~2-3x cheaper
  than an RGB one.
- Stationary objects are not absorbed into the background while they move,
  so a standing person keeps triggering. After `tracker.staticAfter` seconds
  without significant movement a track is absorbed into the background model
  instead (parked car, moved furniture); when the object later leaves, the
  revealed area produces one more trigger and is absorbed again.
- A near-total foreground (`background.reinitFg` percent, for
  `background.reinitStreak` consecutive frames) is treated as a global scene
  change (light switch, camera move): no triggers are produced and the model
  reinitializes from the current frame.
- Track matching is greedy over all track/detection pairs by descending
  `IoU`. A successor track appearing where a recently dropped track was
  inherits its trigger cooldown, so a broken track does not bypass the
  cooldown.
- At most 16 blobs per frame are tracked; smaller ones are dropped first.
- The detection `score` is the blob solidity: the percent of the bounding box
  filled by foreground pixels. It is not a confidence probability; the label
  is always `object`.

Example:

```json
{
	"objectDetectionCv": {
		"enable": true,
		"feedRate": 5,
		"duration": 60,
		"useSubStream": true,
		"scale": 2,
		"mask": { "enable": true, "area": [[1000000, 0], [1000000, 400000], [600000, 400000], [600000, 0]] },
		"blob": { "minArea": 0.5 }
	}
}
```
