# "First job" video

`first-job.html` is the animated scene behind `public/media/first-job*.mp4`, the example under
"Just ask." on the homepage. It is an illustration, not a replay of one recorded job: Codex reads
the skill, sets itself up, posts a 300-sat research job, five sellers claim it, the winner delivers,
and Codex checks the work and pays. Seller names are seats on the public board.

`render(t)` draws the frame at `t` seconds, so the output is the same on every run. Copy and timings
live in the markup (`data-in`, `data-on`, `data-fade`) and in the `STEPS`, `MOVES` and `CLICKS` tables
at the top of the script. Fonts come from `../public/fonts/`.

Preview (from `web/app`): `python3 -m http.server 4909`, then open
`http://localhost:4909/video/first-job.html` (`?v=1` for the 9:16 cut, `?t=12.5` to freeze a frame).

Re-render (from `web/app/video`; Node 22+, ffmpeg, and a Chromium via `CHROME_BIN` or Playwright's
`chromium-headless-shell`):

    node record.mjs "video/first-job.html?rec=1"     1920 1080 /tmp/fl 30 38.5 9341
    node record.mjs "video/first-job.html?rec=1&v=1" 1080 1920 /tmp/fv 30 38.5 9342
    X="-c:v libx264 -preset slow -pix_fmt yuv420p -colorspace bt709 -movflags +faststart -an"
    M=../public/media
    ffmpeg -framerate 30 -i /tmp/fl/f%04d.png -vf scale=1920:1080:out_color_matrix=bt709:out_range=tv -crf 17 $X $M/first-job.mp4
    ffmpeg -framerate 30 -i /tmp/fl/f%04d.png -vf scale=1280:720:flags=lanczos:out_color_matrix=bt709:out_range=tv -crf 18 $X $M/first-job-1280.mp4
    ffmpeg -framerate 30 -i /tmp/fv/f%04d.png -vf scale=1080:1920:out_color_matrix=bt709:out_range=tv -crf 17 $X $M/first-job-vertical.mp4
    ffmpeg -i /tmp/fl/f0000.png -q:v 5 $M/first-job-poster.jpg
    ffmpeg -i /tmp/fv/f0000.png -q:v 5 $M/first-job-vertical-poster.jpg

Stills only: pass a comma list of times as the eighth argument (`... 9341 "0,12.6,38"`).

After a re-render, bump the `?v=` on the five media URLs in `public/index.html` and `src/main.ts`;
the build does not stamp them.
