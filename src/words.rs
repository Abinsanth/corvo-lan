//! The 256 words that make names unique: `bob` → `bobroger`, `cat` → `catpie`.

/// Every entry is 3–5 lowercase ASCII letters, with no duplicates and none
/// ending another, so a name splits into base and word only one way (see tests).
pub const WORDS: [&str; 256] = [
    "ant", "arch", "ash", "aspen", "atom", "axe", "bay", "bean", "bear", "bee", "bell", "berry", "bird", "bison",
    "blade", "bloom", "boat", "bolt", "brook", "bud", "bunny", "cake", "calm", "camel", "candy", "cargo", "cat",
    "cave", "cedar", "chalk", "charm", "chess", "chip", "cider", "clay", "cliff", "cloud", "clove", "coast", "cobra",
    "cocoa", "comet", "coral", "crab", "crane", "creek", "crow", "cub", "cup", "curry", "daisy", "dawn", "deer",
    "delta", "dew", "dingo", "disco", "dock", "dove", "drum", "duck", "dune", "dusk", "echo", "elk", "elm", "fable",
    "fern", "fig", "finch", "fire", "fjord", "flame", "flint", "flute", "foam", "fog", "forge", "frog", "gecko", "gem",
    "ghost", "glade", "glow", "goat", "gold", "goose", "grape", "grove", "gull", "gust", "hare", "harp", "hawk",
    "hazel", "heron", "hill", "honey", "hood", "iris", "ivy", "jade", "jam", "jazz", "jelly", "jet", "joy", "kale",
    "kayak", "kelp", "kite", "koala", "lake", "lamp", "lark", "lava", "leaf", "lime", "lion", "lotus", "lynx", "mango",
    "maple", "marsh", "melon", "mist", "mole", "moon", "moose", "moss", "moth", "mouse", "mule", "nest", "newt",
    "night", "nova", "oak", "ocean", "olive", "onyx", "orbit", "orca", "otter", "owl", "palm", "panda", "pearl",
    "pecan", "pie", "pike", "pine", "plum", "polar", "pond", "prism", "puma", "quail", "quill", "rain", "raven",
    "reed", "reef", "rice", "ridge", "river", "robin", "rock", "roger", "rose", "ruby", "rust", "sage", "salt", "sand",
    "seal", "shark", "shell", "silk", "sky", "slate", "sloth", "snow", "spark", "star", "stone", "storm", "sun",
    "swan", "tango", "teal", "thorn", "tide", "tiger", "toad", "topaz", "trout", "tulip", "twig", "vine", "viola",
    "wasp", "wave", "whale", "wheat", "wind", "wolf", "wren", "yew", "zebra", "zest", "acorn", "amber", "basil",
    "blaze", "wisp", "yarn", "yoyo", "cove", "dart", "fawn", "gnu", "luffy", "zoro", "nami", "usopp", "sanji", "jinbe",
    "ace", "sabo", "law", "kid", "buggy", "vivi", "kaido", "garp", "coby", "kuma", "enel", "lucci", "oden", "momo",
    "bepo", "zeff", "marco", "teach", "wapol", "kyros", "pedro", "moria", "boa", "sunny", "merry", "pell", "hody",
    "sugar",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn words_are_unique_short_and_lowercase() {
        let unique: HashSet<&str> = WORDS.iter().copied().collect();
        assert_eq!(unique.len(), 256);
        for w in WORDS {
            assert!((3..=5).contains(&w.len()), "{w}");
            assert!(w.chars().all(|c| c.is_ascii_lowercase()), "{w}");
        }
    }

    #[test]
    fn no_word_ends_another() {
        for a in WORDS {
            for b in WORDS {
                assert!(a == b || !b.ends_with(a), "{a} ends {b}");
            }
        }
    }
}
