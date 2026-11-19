//! Veinminer for Horizon: break one ore with a pickaxe and the whole connected vein goes with it.
//!
//! Port of the "Veinmine & Autosmelt Enchantments" datapack's veinminer half. The datapack
//! counted `minecraft.mined` scoreboard stats and flood-filled with recursive functions; here the
//! `break_block` hook starts a flood fill and every block is broken *as the player* with
//! `PlayerRef::destroy_block`, so Fortune, Silk Touch and tool wear come from the game itself
//! instead of being re-implemented.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use mod_api::{
    ActiveEffect, BlockDropsEvent, BreakBlockEvent, BuildCreativeModeTabContentsEvent, ClientHost,
    Component, Config, ConfigFile, ConfigKind, ConfigLoadingEvent, ConfigReloadingEvent, Flow,
    Hooks, HostSide, Identifier, ItemKey, ItemStack, LevelRef, ParticleKey, ParticleOptions,
    RegisterConfigsEvent, Registrar, RegistrationContext, ServerHost, SoundSource, Tag,
};

const MOD_ID: &str = "veinminer_autosmelt";

const DEFAULT_CONFIG: &str = r#"# Veinminer settings (server-wide).

# Give hunger while veinmining (the datapack's "hunger" toggle).
hunger = false

# Ticks between two veinmines by the same player (20 ticks = 1 second).
cooldown = 10

# Only veinmine with a pickaxe that has the Veinmine enchantment.
# The original datapack bundle required it; set false to veinmine with any enabled pickaxe.
require_enchantment = true

# Safety cap on how many blocks one veinmine may break.
max_vein_size = 128

# Which pickaxes may veinmine.
wooden_pickaxe = true
stone_pickaxe = true
copper_pickaxe = true
iron_pickaxe = true
golden_pickaxe = true
diamond_pickaxe = true
netherite_pickaxe = true
"#;

/// Which vein a block belongs to. Regular, deepslate and (for iron, copper, gold) raw-block
/// variants of one ore are one family, so a vein continues across them.
fn family(short: &str) -> Option<&'static str> {
    Some(match short {
        "coal_ore" | "deepslate_coal_ore" => "coal",
        "iron_ore" | "deepslate_iron_ore" | "raw_iron_block" => "iron",
        "copper_ore" | "deepslate_copper_ore" | "raw_copper_block" => "copper",
        "gold_ore" | "deepslate_gold_ore" | "raw_gold_block" => "gold",
        "redstone_ore" | "deepslate_redstone_ore" => "redstone",
        "emerald_ore" | "deepslate_emerald_ore" => "emerald",
        "lapis_ore" | "deepslate_lapis_ore" => "lapis",
        "diamond_ore" | "deepslate_diamond_ore" => "diamond",
        "nether_gold_ore" => "nether_gold",
        "nether_quartz_ore" => "nether_quartz",
        _ => return None,
    })
}

/// Whether breaking this block with a Smelting pickaxe has something to smelt. An ore only drops
/// raw metal without Silk Touch; a raw block always drops itself.
fn smeltable(short: &str, silk_touch: bool) -> bool {
    match short {
        "raw_iron_block" | "raw_copper_block" | "raw_gold_block" => true,
        "iron_ore" | "deepslate_iron_ore" | "copper_ore" | "deepslate_copper_ore" | "gold_ore"
        | "deepslate_gold_ore" => !silk_touch,
        _ => false,
    }
}

/// What a Smelting pickaxe turns drops into (the datapack's `smelt_items` list).
const SMELTS: &[(&str, &str)] = &[
    ("minecraft:raw_copper", "minecraft:copper_ingot"),
    ("minecraft:raw_copper_block", "minecraft:copper_block"),
    ("minecraft:raw_gold", "minecraft:gold_ingot"),
    ("minecraft:raw_gold_block", "minecraft:gold_block"),
    ("minecraft:raw_iron", "minecraft:iron_ingot"),
    ("minecraft:raw_iron_block", "minecraft:iron_block"),
];

const VEINMINE: &str = "veinminer_autosmelt:veinminer";
const SMELTING: &str = "veinminer_autosmelt:smelting";

const PICKAXES: &[&str] = &[
    "wooden_pickaxe",
    "stone_pickaxe",
    "copper_pickaxe",
    "iron_pickaxe",
    "golden_pickaxe",
    "diamond_pickaxe",
    "netherite_pickaxe",
];

#[derive(Clone, Debug)]
struct Settings {
    hunger: bool,
    cooldown: i64,
    require_enchantment: bool,
    max_vein_size: usize,
    pickaxes: HashMap<String, bool>,
}

impl Default for Settings {
    fn default() -> Self {
        Self::parse(DEFAULT_CONFIG)
    }
}

impl Settings {
    /// Reads the flat `key = value` lines of the config; unknown or malformed lines are skipped.
    fn parse(text: &str) -> Self {
        let mut s = Settings {
            hunger: false,
            cooldown: 10,
            require_enchantment: true,
            max_vein_size: 128,
            pickaxes: PICKAXES.iter().map(|p| ((*p).to_owned(), true)).collect(),
        };
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "hunger" => s.hunger = value == "true",
                "require_enchantment" => s.require_enchantment = value != "false",
                "cooldown" => s.cooldown = value.parse().unwrap_or(s.cooldown).max(0),
                "max_vein_size" => {
                    s.max_vein_size = value.parse().unwrap_or(s.max_vein_size).max(1)
                }
                other if PICKAXES.contains(&other) => {
                    s.pickaxes.insert(other.to_owned(), value != "false");
                }
                _ => {}
            }
        }
        s
    }
}

/// An enchantment list component on a stack (`minecraft:enchantments`, or a book's
/// `minecraft:stored_enchantments`), as `(enchantment id, level)`. Reads both the current flat
/// form (`{"minecraft:fortune": 3}`) and the older `{levels: {...}}` form.
struct EnchantList(&'static str);

const ENCHANTMENTS: EnchantList = EnchantList("minecraft:enchantments");
const STORED_ENCHANTMENTS: EnchantList = EnchantList("minecraft:stored_enchantments");

impl Component for EnchantList {
    type Value = Vec<(String, i32)>;
    fn id(&self) -> &str {
        self.0
    }
    fn decode(&self, value: &Tag) -> Option<Self::Value> {
        let Tag::Compound(root) = value else {
            return None;
        };
        let levels = match root.get(&"levels".into()) {
            Some(Tag::Compound(inner)) => inner,
            _ => root,
        };
        Some(
            levels
                .iter()
                .filter_map(|(k, v)| {
                    let level = match v {
                        Tag::Int(n) => *n,
                        Tag::Short(n) => i32::from(*n),
                        Tag::Byte(n) => i32::from(*n),
                        _ => return None,
                    };
                    Some((k.to_string().ok()?, level))
                })
                .collect(),
        )
    }
    fn encode(&self, value: Self::Value) -> Tag {
        Tag::Compound(
            value
                .into_iter()
                .map(|(k, v)| (k.as_str().into(), Tag::Int(v)))
                .collect(),
        )
    }
}

#[derive(Default)]
struct Veinminer {
    settings: Mutex<Settings>,
    /// True while our own breaks run, so they are not veinmined again.
    busy: AtomicBool,
    vein: Mutex<HashSet<[i32; 3]>>,
    kept: Mutex<(Vec<ItemStack>, u32)>,
    /// Game tick of each player's last veinmine (the datapack's `cooldown` score).
    last_used: Mutex<HashMap<u128, i64>>,
    /// Raw item -> smelted item, resolved from the registry on the first break.
    smelts: Mutex<Vec<(ItemKey, ItemKey)>>,
    /// The smoke particle, requested at registration.
    smoke: Mutex<Option<ParticleKey>>,
}

fn has_enchantment(stack: &ItemStack, wanted: &str) -> bool {
    stack
        .get(&ENCHANTMENTS)
        .is_some_and(|list| list.iter().any(|(id, level)| id == wanted && *level > 0))
}

impl Veinminer {
    /// The datapack's smelt effect: a fire-extinguish hiss and a puff of smoke
    /// (`particle smoke ~ ~.25 ~ .25 .25 .25 0.1 10`).
    fn smelt_effects(&self, host: &mut dyn ServerHost, level: LevelRef, pos: [i32; 3], sound: bool) {
        let [x, y, z] = pos.map(|v| f64::from(v) + 0.5);
        if sound {
            let _ = level.play_sound(
                host,
                "minecraft:block.fire.extinguish",
                [x, y, z],
                SoundSource::Blocks,
                1.0,
                2.0,
            );
        }
        let smoke = *self.smoke.lock().unwrap();
        if let Some(smoke) = smoke {
            let _ = level.send_particles(
                host,
                &ParticleOptions::simple(smoke),
                None,
                [x, y - 0.25, z],
                [0.25, 0.25, 0.25],
                0.1,
                10,
            );
        }
    }

    fn apply_config(&self, config: &ConfigFile) {
        if config.mod_id == MOD_ID {
            *self.settings.lock().unwrap() = Settings::parse(&config.text);
        }
    }
}

#[mod_api::horizon_mod]
impl Hooks for Veinminer {
    fn register(&self, r: &mut dyn Registrar, _: &RegistrationContext) {
        *self.smoke.lock().unwrap() = Some(r.particle_key("minecraft:smoke"));
    }

    fn register_configs(&self, _: HostSide, event: &mut RegisterConfigsEvent<'_>) {
        event
            .configs
            .push(Config::new(MOD_ID, ConfigKind::Common, DEFAULT_CONFIG));
    }

    fn config_loading(&self, _: HostSide, event: &mut ConfigLoadingEvent<'_>) {
        self.apply_config(event.config);
    }

    fn config_reloading(&self, _: HostSide, event: &mut ConfigReloadingEvent<'_>) {
        self.apply_config(event.config);
    }

    fn break_block(&self, host: &mut dyn ServerHost, event: &mut BreakBlockEvent<'_>) -> Flow {
        {
            let mut smelts = self.smelts.lock().unwrap();
            if smelts.is_empty() {
                *smelts = SMELTS
                    .iter()
                    .filter_map(|(raw, done)| {
                        let key = |id: &str| host.item_key(&Identifier::try_from(id.to_owned()).ok()?);
                        Some((key(raw)?, key(done)?))
                    })
                    .collect();
            }
        }
        // Our own breaks come through here too.
        if self.busy.load(Ordering::Relaxed) {
            return Flow::Continue;
        }
        let settings = self.settings.lock().unwrap().clone();

        let origin_block = event.state.block();
        let block_id = origin_block.id(host);
        let short = block_id.strip_prefix("minecraft:").unwrap_or("");
        let Some(origin_family) = family(short) else {
            return Flow::Continue;
        };

        let Some(player) = host.players().acting_player() else {
            return Flow::Continue;
        };

        // The held tool.
        let Some(slot) = player.selected_slot(host) else {
            return Flow::Continue;
        };
        let held = player
            .inventory_slots(host)
            .and_then(|slots| slots.into_iter().find(|(i, _)| *i as usize == slot))
            .map(|(_, stack)| stack);
        let Some(held) = held else {
            return Flow::Continue;
        };
        let Some(level) = player.level(host) else {
            return Flow::Continue;
        };
        let smelting = has_enchantment(&held, SMELTING);
        let silk_touch = has_enchantment(&held, "minecraft:silk_touch");

        // May this break veinmine? Pickaxe toggle, enchantment, cooldown.
        let item_id = held.item().id(host);
        let pick = item_id.strip_prefix("minecraft:").unwrap_or("");
        let now = level.game_time(host).unwrap_or(0);
        let uuid = player.uuid();
        let on_cooldown = self
            .last_used
            .lock()
            .unwrap()
            .get(&uuid)
            .is_some_and(|last| now - last < settings.cooldown);
        let veinmine = settings.pickaxes.get(pick).copied().unwrap_or(false)
            && (!settings.require_enchantment || has_enchantment(&held, VEINMINE))
            && !on_cooldown;

        // Flood fill over all 26 neighbours (faces, edges and corners) of the same ore family,
        // nearest first, origin first.
        let origin = event.pos;
        let mut order = vec![origin];
        let mut seen: HashSet<[i32; 3]> = HashSet::from([origin]);
        if veinmine {
            let mut queue = VecDeque::from([origin]);
            'fill: while let Some(at) = queue.pop_front() {
                for dx in -1..=1 {
                    for dy in -1..=1 {
                        for dz in -1..=1 {
                            if dx == 0 && dy == 0 && dz == 0 {
                                continue;
                            }
                            let next = [at[0] + dx, at[1] + dy, at[2] + dz];
                            if seen.contains(&next) {
                                continue;
                            }
                            let Some(key) = level.block(host, next) else {
                                continue;
                            };
                            let id = key.id(host);
                            let same = key == origin_block
                                || family(id.strip_prefix("minecraft:").unwrap_or(""))
                                    == Some(origin_family);
                            if same {
                                seen.insert(next);
                                order.push(next);
                                queue.push_back(next);
                                if order.len() >= settings.max_vein_size {
                                    break 'fill;
                                }
                            }
                        }
                    }
                }
            }
        }
        if order.len() < 2 {
            // A lone ore: the ordinary break happens, with the smelt effect if it smelts.
            if smelting && smeltable(short, silk_touch) {
                self.smelt_effects(host, level, origin, true);
            }
            return Flow::Continue;
        }

        self.busy.store(true, Ordering::Relaxed);
        *self.vein.lock().unwrap() = seen;
        let mut hissed = false;
        for pos in &order {
            if smelting {
                let id = level
                    .block(host, *pos)
                    .map(|key| key.id(host))
                    .unwrap_or_default();
                if smeltable(id.strip_prefix("minecraft:").unwrap_or(""), silk_touch) {
                    // One hiss per veinmine, but smoke at every block.
                    self.smelt_effects(host, level, *pos, !hissed);
                    hissed = true;
                }
            }
            player.destroy_block(host, *pos, false);
        }
        self.busy.store(false, Ordering::Relaxed);
        self.vein.lock().unwrap().clear();
        self.last_used.lock().unwrap().insert(uuid, now);

        if settings.hunger {
            if let Some(hunger) = host.effect_key("minecraft:hunger") {
                if let Some(living) = host.living(player.entity()) {
                    let _ = living
                        .add_effect(host, &ActiveEffect::new(hunger, 40, 120, true, true, true));
                }
            }
        }

        // Gather everything at the block the player mined, merged within stack sizes.
        let (stacks, experience) = std::mem::take(&mut *self.kept.lock().unwrap());
        let mut merged: Vec<ItemStack> = Vec::new();
        for stack in stacks {
            let max = host.max_stack_size(stack.item()).unwrap_or(64);
            let mut rest = stack.count();
            for into in merged.iter_mut().filter(|m| m.is_stackable_with(&stack)) {
                let room = max.saturating_sub(into.count()).min(rest);
                let _ = into.grow(room);
                rest -= room;
            }
            if rest > 0 {
                let mut left = stack.clone();
                let _ = left.set_count(rest);
                merged.push(left);
            }
        }
        for stack in merged {
            let _ = level.pop_resource(host, origin, stack);
        }
        let [x, y, z] = origin.map(|v| f64::from(v) + 0.5);
        level.award_experience(host, x, y, z, experience);

        // The origin was broken by the loop above.
        Flow::Cancel
    }

    fn block_drops(&self, _: &dyn mod_api::LevelHost, event: &mut BlockDropsEvent<'_>) -> Flow {
        // Smelting: raw iron, gold and copper (and their blocks) drop as ingots and blocks.
        if event.tool.is_some_and(|tool| has_enchantment(tool, SMELTING)) {
            let smelts = self.smelts.lock().unwrap();
            for drop in event.drops.iter_mut() {
                if let Some((_, done)) = smelts.iter().find(|(raw, _)| *raw == drop.stack.item()) {
                    if let Some(mut converted) = ItemStack::new(*done, drop.stack.count()) {
                        std::mem::swap(&mut drop.stack, &mut converted);
                    }
                }
            }
        }
        if self.busy.load(Ordering::Relaxed)
            && self
                .vein
                .lock()
                .unwrap()
                .contains(&[event.pos.x, event.pos.y, event.pos.z])
        {
            let mut kept = self.kept.lock().unwrap();
            kept.0.extend(event.drops.drain(..).map(|drop| drop.stack));
            kept.1 += std::mem::take(&mut event.experience);
        }
        Flow::Continue
    }

    /// Put both enchanted books where vanilla's are, on the Ingredients tab (and so in search).
    fn build_creative_mode_tab_contents(
        &self,
        host: &mut dyn ClientHost,
        event: &mut BuildCreativeModeTabContentsEvent<'_>,
    ) {
        if event.tab != "minecraft:ingredients" {
            return;
        }
        let Some(book) = Identifier::try_from("minecraft:enchanted_book".to_owned())
            .ok()
            .and_then(|id| host.item_key(&id))
        else {
            return;
        };
        for enchantment in [VEINMINE, SMELTING] {
            let mut stack = ItemStack::of(book);
            if stack
                .set(&STORED_ENCHANTMENTS, vec![(enchantment.to_owned(), 1)])
                .is_ok()
            {
                event.items.push(stack);
            }
        }
    }
}
