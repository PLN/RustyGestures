# Clean-room rules

RustyGestures is written from public behavioural descriptions, the published gesture papers, and Microsoft's documentation. Contributors keep it that way.

## Allowed sources

- Public behaviour of StrokesPlus.net: the changelog, the forum export, and the archived help pages.
- The published algorithms: Protractor (Li, CHI 2010), $1 (Wobbrock, Wilson, and Li, UIST 2007), and the 1D scaling note in $N (Anthony and Wobbrock, 2010).
- Microsoft documentation for Win32, the low-level hooks, SendInput, DPI awareness, DirectComposition, and Direct2D.

## Not allowed

- Decompiling, disassembling, unpacking, or debugging StrokesPlus.net binaries, or reading their strings or resources for implementation detail.
- Reading or copying code from minyoad/StrokesPlus, Input.Observer, or any other mouse-gesture or stroke-recognition program.
- Using or adapting an existing $1 or Protractor crate. The recognizer in this repo is written from the papers.
- Studying the paid successor of StrokesPlus.net.
- Claiming compatibility with StrokesPlus.net. There is no shared config, importer, or API.

## Name

Do not use "StrokesPlus", "S+", or "StrokeIt" in branding, crate names, or window titles.

## Review

Every pull request confirms these rules with the clean-room checkbox in the pull-request template. Recognition changes should be explainable from the papers. Hooking and injection changes should be explainable from Microsoft's docs.
