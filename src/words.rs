//! The 256 words that make names unique: `bob` → `bobRoger`, `cat` → `catPieR`.

/// Every entry is 3–5 ASCII letters, capitalised, with no duplicates (see tests).
pub const WORDS: [&str; 256] = [
    "Ant", "Arch", "Ash", "Aspen", "Atom", "Axe", "Bay", "Bean", "Bear", "Bee", "Bell", "Berry", "Bird", "Bison",
    "Blade", "Bloom", "Boat", "Bolt", "Brook", "Bud", "Bunny", "Cake", "Calm", "Camel", "Candy", "Cargo", "Cat",
    "Cave", "Cedar", "Chalk", "Charm", "Chess", "Chip", "Cider", "Clay", "Cliff", "Cloud", "Clove", "Coast", "Cobra",
    "Cocoa", "Comet", "Coral", "Crab", "Crane", "Creek", "Crow", "Cub", "Cup", "Curry", "Daisy", "Dawn", "Deer",
    "Delta", "Dew", "Dingo", "Disco", "Dock", "Dove", "Drum", "Duck", "Dune", "Dusk", "Echo", "Elk", "Elm", "Fable",
    "Fern", "Fig", "Finch", "Fire", "Fjord", "Flame", "Flint", "Flute", "Foam", "Fog", "Forge", "Frog", "Gecko", "Gem",
    "Ghost", "Glade", "Glow", "Goat", "Gold", "Goose", "Grape", "Grove", "Gull", "Gust", "Hare", "Harp", "Hawk",
    "Hazel", "Heron", "Hill", "Honey", "Hood", "Iris", "Ivy", "Jade", "Jam", "Jazz", "Jelly", "Jet", "Joy", "Kale",
    "Kayak", "Kelp", "Kite", "Koala", "Lake", "Lamp", "Lark", "Lava", "Leaf", "Lime", "Lion", "Lotus", "Lynx", "Mango",
    "Maple", "Marsh", "Melon", "Mist", "Mole", "Moon", "Moose", "Moss", "Moth", "Mouse", "Mule", "Nest", "Newt",
    "Night", "Nova", "Oak", "Ocean", "Olive", "Onyx", "Orbit", "Orca", "Otter", "Owl", "Palm", "Panda", "Pearl",
    "Pecan", "Pie", "Pike", "Pine", "Plum", "Polar", "Pond", "Prism", "Puma", "Quail", "Quill", "Rain", "Raven",
    "Reed", "Reef", "Rice", "Ridge", "River", "Robin", "Rock", "Roger", "Rose", "Ruby", "Rust", "Sage", "Salt", "Sand",
    "Seal", "Shark", "Shell", "Silk", "Sky", "Slate", "Sloth", "Snow", "Spark", "Star", "Stone", "Storm", "Sun",
    "Swan", "Tango", "Teal", "Thorn", "Tide", "Tiger", "Toad", "Topaz", "Trout", "Tulip", "Twig", "Vine", "Viola",
    "Wasp", "Wave", "Whale", "Wheat", "Wind", "Wolf", "Wren", "Yew", "Zebra", "Zest", "Acorn", "Amber", "Basil",
    "Blaze", "Wisp", "Yarn", "Yoyo", "Cove", "Dart", "Fawn", "Gnu", "Luffy", "Zoro", "Nami", "Usopp", "Sanji", "Jinbe",
    "Ace", "Sabo", "Law", "Kid", "Buggy", "Vivi", "Kaido", "Garp", "Coby", "Kuma", "Enel", "Lucci", "Oden", "Momo",
    "Bepo", "Zeff", "Marco", "Teach", "Wapol", "Kyros", "Pedro", "Moria", "Boa", "Sunny", "Merry", "Pell", "Hody",
    "Sugar",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn words_are_unique_short_and_capitalised() {
        let unique: HashSet<&str> = WORDS.iter().copied().collect();
        assert_eq!(unique.len(), 256);
        for w in WORDS {
            assert!((3..=5).contains(&w.len()), "{w}");
            let mut chars = w.chars();
            assert!(chars.next().unwrap().is_ascii_uppercase(), "{w}");
            assert!(chars.all(|c| c.is_ascii_lowercase()), "{w}");
        }
    }
}
