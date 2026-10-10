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
| `mask.enable` | `false` | Ignore blobs whose center is inside the mask polygon |
| `mask.area` | `[]` | Normalized polygon points (`1_000_000` = 100%) |
| `background.samples` | `8` | Background samples per pixel |
| `background.threshold` | `20` | Luma distance for a sample to count as a match |
| `background.minMatch` | `2` | Matching samples required for a pixel to be background |
| `background.updateRate` | `16` | 1 in N chance to update a background pixel model |
| `blob.minArea` | `0.2` | Minimum blob area, percent of the frame |
| `blob.maxArea` | `50` | Maximum blob area, percent of the frame |
| `blob.minAspect` | `0.1` | Minimum blob width / height ratio |
| `blob.maxAspect` | `10` | Maximum blob width / height ratio |
| `blob.open` | `true` | 3x3 opening, removes single-pixel speckle |
| `blob.close` | `true` | 3x3 closing, fills small holes |
| `tracker.minHits` | `2` | Consecutive frames before a track is confirmed |
| `tracker.maxAge` | `5` | Missed frames before a track is dropped |
| `tracker.iou` | `0.3` | Minimum IoU to associate a detection with a track |
| `tracker.cooldown` | `5` | Seconds before the same track may trigger again. Derived from the feed rate; a feed rate of `0` disables it |

Notes:

- Only the luma (Y) plane is used; chroma is ignored, which matches how the
  other detectors in this project work and keeps the pipeline ~2-3x cheaper
  than an RGB one.
- Stationary objects are not absorbed into the background: foreground pixels
  never update the model. A near-total foreground persisting for ~12 frames
  (camera move, light switch) reinitializes the model instead.
- At most 16 blobs per frame are tracked; smaller ones are dropped first.
- The detection `score` is the blob solidity: the percent of the bounding box
  filled by foreground pixels.

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
