#!/usr/bin/env python3
"""Generate `data/bot_names.json` — the pool of usernames the organic-bot system draws from.

Bots must be indistinguishable from humans, so the names read like real handles a person would
pick: edgy internet / gaming humor, deliberately a bit cringe, but BRAND-SAFE (no slurs, no
profanity, no targeted harassment). Every name obeys the same rules the human registration path
enforces (`src/api.rs`): 3–20 chars, `[A-Za-z0-9_-]` only.

Run:  python scripts/build_bot_names.py        (writes data/bot_names.json, then rebuild the crate)

Deterministic (seeded) so re-running yields the same list — keep it in version control.
"""
import json
import os
import random
import re

SEED = 1312
TARGET = 1000
VALID = re.compile(r"^[A-Za-z0-9_-]{3,20}$")

# Hand-picked, memorable handles set the tone. These go in first.
CURATED = [
    "xX_n0scope_Xx", "touch_grass_l8r", "404_skill_lost", "rm_rf_my_life", "segfault_sally",
    "certified_hood", "feralhog30to50", "moist_owlette", "sigma_grindset", "copium_addict",
    "ratiod_again", "doomer_42", "bonk_police", "skibidi_ohio", "gigachad_andy", "sus_imposter",
    "yeet_machine", "based_dept", "npc_dialogue", "ohio_final_boss", "rizzler_69", "gyatt_damn",
    "menace2sobriety", "chronically_online", "terminally_silly", "professional_yapper",
    "unemployed_king", "wage_cage_andy", "404_brain", "ctrl_alt_defeat", "git_gud_scrub",
    "lag_switch_larry", "ping_of_death", "afk_in_spawn", "clutch_or_kick", "whiffed_again",
    "baited_lol", "malding_irl", "seething_coping", "no_maidens", "down_horrendous",
    "stinky_gamer", "goblin_mode_on", "raccoon_rights", "possum_patrol", "pigeon_overlord",
    "frog_in_a_bog", "wizard_post", "cowboy_kim", "intern_no_4", "wagie_cagie", "sweat_lord",
    "tryhard_timmy", "camper_steve", "griefer_greg", "tilted_tom", "noob_slayer_xd", "pro_gamer_move",
    "1v1_me_bro", "ez_clap", "gg_no_re", "mid_diff", "skill_issue", "hard_stuck", "elo_hell_local",
    "smurf_account", "alt_f4_warrior", "respawn_addict", "loot_goblin", "crit_happens",
    "aggro_andy", "tank_diff", "heal_pls", "out_of_mana", "low_battery_lad", "buffer_ing",
    "rage_quitter", "spawn_camper_x", "one_more_game", "just_one_more", "sleep_is_optional",
    "caffeine_dependent", "snack_break_now", "404_motivation", "vibe_check_failed", "no_thoughts",
    "head_empty", "brain_lag", "loading_forever", "buffering99", "the_real_slim_shady_no",
    "definitely_human", "totally_not_a_bot", "real_person_btw", "average_enjoyer", "local_menace",
    "your_local_npc", "background_char", "side_quest_andy", "tutorial_island", "patch_notes_enjoyer",
    "nerf_this", "buff_me_pls", "meta_slave", "off_meta_andy", "jank_build_only", "glass_cannon_dan",
]

# Combinatorial pools — meme-y but clean.
ADJ = [
    "based", "sus", "feral", "sigma", "sweaty", "toxic", "cursed", "chad", "doomer", "goofy",
    "silly", "spicy", "crusty", "moist", "dank", "epic", "tryhard", "tilted", "malding", "seething",
    "coping", "unhinged", "chronically", "terminally", "mid", "cringe", "goblin", "gremlin",
    "stinky", "smelly", "lonely", "broke", "rich", "tiny", "giant", "fast", "slow", "lucky",
    "cursed", "blessed", "haunted", "drippy", "saucy", "zesty", "thicc", "bonky", "wacky", "spooky",
]
ADJ = [a for a in ADJ if a != "thicc"]  # keep it PG-13

NOUN = [
    "andy", "chad", "enjoyer", "gamer", "goblin", "gremlin", "menace", "warlord", "overlord",
    "peasant", "raccoon", "possum", "pigeon", "crow", "frog", "toad", "wizard", "knight", "bandit",
    "outlaw", "cowboy", "sheriff", "captain", "admiral", "general", "intern", "manager", "wagie",
    "drone", "worker", "soldier", "viking", "pirate", "ninja", "samurai", "imposter", "crewmate",
    "noodle", "potato", "pickle", "biscuit", "nugget", "muffin", "gremlin", "wizard", "lich",
    "demon", "angel", "ghost", "zombie", "skeleton", "vampire", "goose", "duck", "moth", "slug",
]

SUFFIX = [
    "69", "420", "42", "007", "99", "2000", "9000", "1337", "xd", "uwu", "ttv", "yt", "jr", "sr",
    "irl", "real", "official", "tm", "xx", "btw", "lol", "gg", "ez", "pro", "noob", "main", "alt",
]

LEET = {"o": "0", "i": "1", "e": "3", "a": "4", "s": "5", "t": "7"}


def leetify(word, rng):
    return "".join(LEET.get(c, c) if rng.random() < 0.35 else c for c in word)


def generate():
    rng = random.Random(SEED)
    names = []
    seen = set()

    def add(n):
        key = n.lower()
        if VALID.match(n) and key not in seen:
            seen.add(key)
            names.append(n)
            return True
        return False

    for n in CURATED:
        add(n)

    patterns = [
        lambda r: f"{r.choice(ADJ)}_{r.choice(NOUN)}",
        lambda r: f"{r.choice(ADJ)}{r.choice(NOUN)}",
        lambda r: f"{r.choice(NOUN)}_{r.choice(SUFFIX)}",
        lambda r: f"{r.choice(ADJ)}_{r.choice(NOUN)}_{r.choice(SUFFIX)}",
        lambda r: f"{r.choice(ADJ)}{r.choice(NOUN)}{r.choice(SUFFIX)}",
        lambda r: f"xX_{r.choice(NOUN)}_Xx",
        lambda r: f"{r.choice(NOUN)}_{r.choice(NOUN)}",
        lambda r: f"{r.choice(ADJ)}_{leetify(r.choice(NOUN), r)}",
        lambda r: f"{leetify(r.choice(ADJ), r)}_{r.choice(NOUN)}{r.choice(SUFFIX)}",
        lambda r: f"the_{r.choice(ADJ)}_{r.choice(NOUN)}",
    ]

    # Bounded attempts so a saturated namespace can't loop forever.
    attempts = 0
    while len(names) < TARGET and attempts < TARGET * 200:
        attempts += 1
        add(rng.choice(patterns)(rng))

    rng.shuffle(names)
    return names[:TARGET]


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    out = os.path.join(here, "..", "data", "bot_names.json")
    names = generate()
    assert len(names) == TARGET, f"only generated {len(names)} names"
    assert all(VALID.match(n) for n in names), "a name failed validation"
    assert len(set(n.lower() for n in names)) == TARGET, "case-insensitive duplicate slipped in"
    with open(os.path.normpath(out), "w", encoding="utf-8") as f:
        json.dump(names, f, indent=0, ensure_ascii=False)
        f.write("\n")
    print(f"wrote {len(names)} bot names -> {os.path.normpath(out)}")


if __name__ == "__main__":
    main()
