# "Just ask" video

`just-ask.html` is the animated scene behind `public/media/just-ask*.mp4`. It replays one real
market job (offer `fdf9bd80…`, 2026-09-24 15:52:22 UTC: a 100-sat review of PR #1038, claimed and
awarded at +0s, delivered at +302s, verified and paid at +304s). Every time and figure in it comes
from those relay events.

Re-render (serve this folder's parent, then capture frames and encode):

    python3 -m http.server 4909 --directory ..   # from web/app/video
    node record.mjs http://localhost:4909/video/just-ask.html 1280 720 frames 30 30
    node record.mjs http://localhost:4909/video/just-ask.html 720 900 frames-m 30 30
    ffmpeg -framerate 30 -i frames/f%04d.jpg -c:v libx264 -crf 26 -pix_fmt yuv420p -movflags +faststart -an ../public/media/just-ask.mp4
