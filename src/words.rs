//! The 256 words that make names unique: `bob` → `bobRoger`, `cat` → `catPieR`.

/// Every entry is 3–5 ASCII letters, capitalised, with no duplicates (see tests).
pub const WORDS: [&str; 256] = [
    "Ant", "Ape", "Arch", "Ash", "Aspen", "Atom", "Axe", "Bay", "Bean", "Bear", "Bee", "Bell", "Berry", "Birch",
    "Bird", "Bison", "Blade", "Bloom", "Boat", "Bolt", "Bone", "Book", "Brook", "Bud", "Bug", "Bunny", "Cake", "Calm",
    "Camel", "Candy", "Cape", "Cargo", "Cat", "Cave", "Cedar", "Chalk", "Charm", "Chess", "Chip", "Cider", "Clay",
    "Cliff", "Cloud", "Clove", "Coal", "Coast", "Cobra", "Cocoa", "Comet", "Coral", "Corn", "Crab", "Crane", "Creek",
    "Crow", "Crown", "Cub", "Cup", "Curry", "Daisy", "Dawn", "Deer", "Delta", "Dew", "Dingo", "Disco", "Dock", "Dove",
    "Drum", "Duck", "Dune", "Dusk", "Eagle", "Echo", "Elk", "Elm", "Ember", "Fable", "Fern", "Fig", "Finch", "Fire",
    "Fjord", "Flame", "Flint", "Flute", "Foam", "Fog", "Forge", "Fox", "Frog", "Frost", "Gale", "Gecko", "Gem",
    "Ghost", "Glade", "Glow", "Goat", "Gold", "Goose", "Grape", "Grove", "Gull", "Gust", "Hare", "Harp", "Hawk",
    "Hazel", "Heron", "Hill", "Honey", "Hood", "Horn", "Ice", "Iris", "Ivy", "Jade", "Jam", "Jay", "Jazz", "Jelly",
    "Jet", "Joy", "Kale", "Kayak", "Kelp", "Kite", "Kiwi", "Koala", "Lake", "Lamp", "Lark", "Lava", "Leaf", "Lemon",
    "Lilac", "Lily", "Lime", "Lion", "Lotus", "Lynx", "Mango", "Maple", "Marsh", "Melon", "Mint", "Mist", "Mole",
    "Moon", "Moose", "Moss", "Moth", "Mouse", "Mule", "Nest", "Newt", "Night", "Nova", "Nut", "Oak", "Oasis", "Ocean",
    "Olive", "Onyx", "Opal", "Orbit", "Orca", "Otter", "Owl", "Palm", "Panda", "Pearl", "Pecan", "Pie", "Pike", "Pine",
    "Plum", "Polar", "Pond", "Pony", "Poppy", "Prism", "Puma", "Quail", "Quill", "Rain", "Raven", "Reed", "Reef",
    "Rice", "Ridge", "River", "Robin", "Rock", "Roger", "Rose", "Ruby", "Rust", "Sage", "Salt", "Sand", "Seal",
    "Shark", "Shell", "Silk", "Sky", "Slate", "Sloth", "Snow", "Sol", "Spark", "Star", "Stone", "Storm", "Sun", "Swan",
    "Tango", "Tea", "Teal", "Thorn", "Tide", "Tiger", "Toad", "Topaz", "Trout", "Tulip", "Tuna", "Twig", "Vale",
    "Vine", "Viola", "Wasp", "Wave", "Whale", "Wheat", "Wind", "Wolf", "Wren", "Yak", "Yew", "Zebra", "Zinc", "Zest",
    "Acorn", "Amber", "Basil", "Blaze", "Wisp", "Yarn", "Yoyo", "Zoom", "Cove", "Dart", "Fawn", "Gnu",
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
