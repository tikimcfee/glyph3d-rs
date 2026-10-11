// Tab stops (2026-10-11): every tab here advances to the next stop of
// --tab-width display cells, counted from the start of its line.
fn main() {
	let x = 1;	// one tab of code, then a comment column
	if x == 1 {		// two tabs
		println!("café	naïve");	// a tab after a non-ASCII glyph
		let été = "世界";	// a CJK glyph (a one-cell miss) before the tab
	}
}

🚀	rocket first, then a tab
1️⃣	keycap, tab	👨‍👩‍👧	family, tab
a	bb	ccc	dddd	eeeee	ffffff	ggggggg	hhhhhhhh	|
				four tabs from the line start
mixed  	 spaces	 	tabs 		|
