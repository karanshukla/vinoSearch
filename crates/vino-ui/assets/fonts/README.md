Inter-{Regular,Medium,SemiBold,Bold}.ttf are static instances of
vinowhisper-landing's `inter-latin.woff2` (Inter 4, Latin subset) at weights
400, 500, 600 and 700, made with fontTools' varLib.instancer. Regular and Bold
replace the identical pair the three apps each bundled before.

JetBrainsMono-Regular.ttf is JetBrains Mono 2.304 Regular, subset to the
same Latin range with pyftsubset. Monospace is for paths and commands only;
everything else is Inter.

Neither subset has arrows or other symbols outside Latin: egui falls back
to its own fonts for those, and a glyph no font has panics in a test, so
check before putting a symbol in a window. "…", "•", "·" and "×" are safe.
