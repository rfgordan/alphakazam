// Co-simulation recorder (verification v2).
//
// Plays a PS battle through the *synchronous* Battle API and records, at every decision point:
//   - the request JSONs PS sent (the legality ground truth),
//   - the choices made (with a `resolved` form: absolute slots + move/species ids, so the
//     Rust side never re-interprets PS's positional choice strings),
//   - every PRNG draw consumed resolving those choices, with semantic labels
//     (the shared "outcome alphabet" both engines speak),
//   - the FULL serialized battle state (State.serializeBattle — nothing hand-projected).
//
// The Rust cosim crate replays these and demands exact equality on all modeled fields,
// classifying every turn matched / diverged / unsupported. Artifacts are stamped with the
// pinned PS commit; this script refuses to run against an unpinned clone.
//
// Usage: node harness/cosim.mjs --seed 1 --games 1 --out harness/cosim-traces/c1.json
//        node harness/cosim.mjs --seed 1 --teamset ou --max-decisions 400

import path from 'node:path';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { assertPsPinned, PS_DIR } from './check-ps-pin.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

const PS_COMMIT = assertPsPinned();
const { Battle, Teams } = require(path.join(PS_DIR, 'dist/sim'));
const { State } = require(path.join(PS_DIR, 'dist/sim/state'));

// ---- args -------------------------------------------------------------------

const argv = process.argv.slice(2);
function arg(name, def) {
	const i = argv.indexOf(`--${name}`);
	return i >= 0 ? argv[i + 1] : def;
}
const SEED_NUM = Number(arg('seed', '1'));
const FORMAT = arg('format', 'gen9customgame');
const TEAMSET = arg('teamset', 'ou');
const MAX_DECISIONS = Number(arg('max-decisions', '600'));
const OUT = arg('out', path.join(__dirname, 'cosim-traces', `c${SEED_NUM}.json`));
const DISTRIBUTIONS = argv.includes('--distributions');
const MAX_DIST_PATHS = Number(arg('max-dist-paths', '250000'));

// ---- teams (packed; unique species per side so idents are unambiguous) -------

const TEAM_OU_1 = [
	'Great Tusk||heavydutyboots|protosynthesis|headlongrush,closecombat,rapidspin,stealthrock|Jolly|,252,,,4,252|||||,,,,,Ground',
	'Gholdengo||choicescarf|goodasgold|makeitrain,shadowball,thunderbolt,focusblast|Timid|,,,252,4,252|||||,,,,,Flying',
	'Kingambit||leftovers|supremeoverlord|kowtowcleave,suckerpunch,ironhead,swordsdance|Adamant|232,252,,,,24|||||,,,,,Dark',
	'Dragapult||heavydutyboots|clearbody|dragondarts,shadowball,uturn,dracometeor|Naive|,84,,172,,252|||||,,,,,Dragon',
	'Corviknight||leftovers|pressure|bravebird,bodypress,roost,uturn|Impish|248,,168,,92,|||||,,,,,Flying',
	'Garganacl||leftovers|purifyingsalt|saltcure,recover,bodypress,earthquake|Careful|252,,4,,252,|||||,,,,,Water',
].join(']');

const TEAM_OU_2 = [
	'Iron Valiant||leftovers|quarkdrive|moonblast,thunderbolt,closecombat,calmmind|Timid|,,,252,4,252|||||,,,,,Fairy',
	'Roaring Moon||heavydutyboots|protosynthesis|dragondance,knockoff,earthquake,acrobatics|Jolly|,252,,,4,252|||||,,,,,Flying',
	'Raging Bolt||leftovers|protosynthesis|thunderclap,dracometeor,thunderbolt,calmmind|Modest|64,,,252,,192|||||,,,,,Electric',
	'Slowking-Galar||leftovers|regenerator|sludgebomb,flamethrower,icebeam,slackoff|Calm|248,,8,,252,|||||,,,,,Water',
	'Dragonite||heavydutyboots|multiscale|dragondance,earthquake,firepunch,roost|Adamant|,252,,,4,252|||||,,,,,Flying',
	'Toxapex||leftovers|regenerator|surf,sludgebomb,recover,haze|Bold|248,252,8,,,|||||,,,,,Steel',
].join(']');

// Directed-coverage teams: deliberately exercise mechanics the OU pair never touches —
// sleep/yawn/para/burn status, screens + Light Clay, weather abilities, Trick Room, tailwind,
// spikes/toxic spikes/web stacks, Leech Seed, Substitute, Encore, Taunt, protect stalling.
const TEAM_DIVERSE_1 = [
	'Torkoal||heatrock|drought|lavaplume,yawn,stealthrock,rapidspin|Bold|248,,252,,8,|||||,,,,,Fire',
	'Amoonguss||rockyhelmet|regenerator|spore,gigadrain,sludgebomb,synthesis|Calm|248,,80,,180,|||||,,,,,Water',
	'Grimmsnarl||lightclay|prankster|reflect,lightscreen,spiritbreak,taunt|Careful|248,,8,,252,|||||,,,,,Steel',
	'Skarmory||rockyhelmet|sturdy|spikes,roost,bodypress,whirlwind|Impish|252,,252,,4,|||||,,,,,Water',
	'Slowking||leftovers|regenerator|trickroom,psychic,chillingwater,slackoff|Relaxed|252,,252,4,,|||||,,,,,Fairy',
	'Garchomp||leftovers|roughskin|substitute,earthquake,swordsdance,dragonclaw|Jolly|,252,,,4,252|||||,,,,,Steel',
].join(']');

const TEAM_DIVERSE_2 = [
	'Pelipper||damprock|drizzle|hydropump,hurricane,roost,uturn|Bold|248,,252,,8,|||||,,,,,Water',
	'Breloom||focussash|technician|machpunch,bulletseed,spore,swordsdance|Adamant|,252,,,4,252|||||,,,,,Fighting',
	'Klefki||lightclay|prankster|thunderwave,spikes,foulplay,lightscreen|Careful|248,,8,,252,|||||,,,,,Water',
	'Whimsicott||focussash|prankster|tailwind,encore,moonblast,memento|Timid|,,,252,4,252|||||,,,,,Fairy',
	'Ninetales-Alola||lightclay|snowwarning|auroraveil,blizzard,moonblast,encore|Timid|,,,252,4,252|||||,,,,,Ice',
	'Toxtricity||throatspray|punkrock|boomburst,overdrive,toxic,shiftgear|Modest|,,,252,4,252|||||,,,,,Normal',
].join(']');

// Trapping-coverage team pairs (t*.json.gz traces, `--teamset trapA`..`trapF`): exercise every
// switch-prevention source and every exemption — trapping abilities (Arena Trap / Shadow Tag /
// Magnet Pull), partial-trap moves with Binding Band / Grip Claw, Mean Look / Block / Jaw Lock /
// Octolock / Ingrain / No Retreat, against grounded, Flying, Ghost, Air Balloon and Shed Shell
// escapes. Victims are deliberately tanky so trapped positions persist across many decision
// points, and no team carries burn/toxic against a trapper (the engine's per-side residual loop
// would order a cross-side poison-death vs partial-trap chip differently than PS's global queue).
const TEAM_TRAP = {
	// Arena Trap vs grounded / Flying / Ghost / Air Balloon.
	trapA: [[
		'Dugtrio||focussash|arenatrap|earthquake,rockslide,suckerpunch,protect|Jolly|,252,,,4,252|||||,,,,,Ground',
		'Golurk||leftovers|shellarmor|earthquake,shadowpunch,icepunch,protect|Adamant|252,252,,,4,|||||,,,,,Ground',
	], [
		'Blissey||leftovers|naturalcure|seismictoss,softboiled,icebeam,protect|Calm|252,,252,,4,|||||,,,,,Normal',
		'Talonflame||heavydutyboots|flamebody|bravebird,roost,tailwind,protect|Jolly|,252,,,4,252|||||,,,,,Fire',
		'Gengar||leftovers|cursedbody|shadowball,thunderbolt,psychic,protect|Timid|,,,252,4,252|||||,,,,,Ghost',
		'Chansey||airballoon|naturalcure|seismictoss,softboiled,thunderwave,protect|Calm|252,,252,,4,|||||,,,,,Normal',
	]],
	// Shadow Tag mirror (mutual immunity) + a trapped bystander.
	trapB: [[
		'Gothitelle||leftovers|shadowtag|psychic,thunderbolt,calmmind,protect|Bold|252,,252,,4,|||||,,,,,Psychic',
		'Skarmory||rockyhelmet|sturdy|bodypress,roost,spikes,whirlwind|Impish|252,,252,,4,|||||,,,,,Steel',
	], [
		'Gothitelle||leftovers|shadowtag|psychic,shadowball,calmmind,protect|Bold|252,,252,,4,|||||,,,,,Psychic',
		'Blissey||leftovers|naturalcure|seismictoss,softboiled,icebeam,protect|Calm|252,,252,,4,|||||,,,,,Normal',
	]],
	// Magnet Pull vs Steel / non-Steel / Steel-with-Shed-Shell.
	trapC: [[
		'Magnezone||leftovers|magnetpull|thunderbolt,flashcannon,thunderwave,protect|Modest|252,,,252,4,|||||,,,,,Electric',
		'Probopass||leftovers|magnetpull|block,flashcannon,thunderwave,protect|Calm|252,,4,,252,|||||,,,,,Rock',
	], [
		'Empoleon||shedshell|torrent|surf,flashcannon,icebeam,protect|Modest|252,,,252,4,|||||,,,,,Water',
		'Scizor||leftovers|technician|bulletpunch,knockoff,swordsdance,protect|Adamant|252,252,,,4,|||||,,,,,Steel',
		'Blissey||leftovers|naturalcure|seismictoss,softboiled,icebeam,protect|Calm|252,,252,,4,|||||,,,,,Normal',
	]],
	// Partial traps: Binding Band Shuckle, Grip Claw Araquanid; Shed Shell / Ghost escapes across.
	trapD: [[
		'Torkoal||bindingband|shellarmor|infestation,sandtomb,rest,protect|Bold|252,,252,,4,|||||,,,,,Fire',
		'Araquanid||gripclaw|waterbubble|whirlpool,bind,liquidation,protect|Adamant|252,252,,,4,|||||,,,,,Water',
	], [
		'Blissey||leftovers|naturalcure|seismictoss,softboiled,calmmind,protect|Calm|252,,252,,4,|||||,,,,,Normal',
		'Corviknight||shedshell|pressure|bodypress,roost,bravebird,protect|Impish|252,,252,,4,|||||,,,,,Flying',
		'Gengar||leftovers|cursedbody|shadowball,psychic,thunderbolt,protect|Timid|,,,252,4,252|||||,,,,,Ghost',
	]],
	// Mean Look / Block, incl. a Ghost target and a Shed Shell holder using Mean Look itself.
	trapE: [[
		'Probopass||leftovers|sturdy|block,flashcannon,thunderwave,protect|Calm|252,,4,,252,|||||,,,,,Rock',
		'Gothitelle||leftovers|competitive|meanlook,psychic,calmmind,protect|Bold|252,,252,,4,|||||,,,,,Psychic',
	], [
		'Blissey||shedshell|naturalcure|meanlook,seismictoss,softboiled,protect|Calm|252,,252,,4,|||||,,,,,Normal',
		'Gengar||leftovers|cursedbody|shadowball,thunderbolt,psychic,protect|Timid|,,,252,4,252|||||,,,,,Ghost',
		'Chansey||leftovers|naturalcure|seismictoss,softboiled,thunderwave,protect|Calm|252,,252,,4,|||||,,,,,Normal',
	]],
	// Jaw Lock (traps BOTH) / Octolock residual drops / Ingrain / No Retreat self-traps.
	trapF: [[
		'Drednaw||leftovers|shellarmor|jawlock,liquidation,crunch,protect|Adamant|252,252,,,4,|||||,,,,,Water',
		'Torkoal||leftovers|shellarmor|octolock,infestation,rest,protect|Bold|252,,252,,4,|||||,,,,,Fire',
	], [
		'Skarmory||rockyhelmet|sturdy|noretreat,bodypress,roost,spikes|Impish|252,,252,,4,|||||,,,,,Steel',
		'Blissey||leftovers|naturalcure|ingrain,seismictoss,softboiled,protect|Calm|252,,252,,4,|||||,,,,,Normal',
		'Gengar||leftovers|cursedbody|shadowball,psychic,thunderbolt,protect|Timid|,,,252,4,252|||||,,,,,Ghost',
	]],
};

// Request-phase mechanics coverage (`c*.json.gz`, `--teamset revive` / `pivot`): moves that
// pause the battle mid-turn for a special switch-like choice, exercised on their real randbats
// carriers (from harness/coverage-worklist.json) with realistic movesets so games flow naturally.
// `revive` carries Revival Blessing (Pawmot/Rabsca) + Shed Tail (Cyclizar/Orthworm) against hard
// hitters, so teammates faint (a Revival Blessing target) and the sub-passing pivot triggers.
// `pivot` carries Chilly Reception (Slowking-Galar/Slowking) + Teleport (Deoxys-D/Solgaleo).
const TEAM_COVERAGE = {
	revive: [[
		'Pawmot||lifeorb|ironfist|closecombat,icepunch,knockoff,revivalblessing|Jolly|,252,,,4,252|||||,,,,,Electric',
		'Rabsca||leftovers|synchronize|psychic,earthpower,recover,revivalblessing|Modest|252,,,252,4,|||||,,,,,Steel',
		'Cyclizar||heavydutyboots|regenerator|dracometeor,knockoff,rapidspin,shedtail|Timid|,,,252,4,252|||||,,,,,Dragon',
		'Orthworm||leftovers|eartheater|bodypress,heavyslam,stealthrock,shedtail|Impish|252,,252,,4,|||||,,,,,Ghost',
		'Blissey||leftovers|naturalcure|seismictoss,softboiled,icebeam,protect|Calm|252,,252,,4,|||||,,,,,Normal',
		'Corviknight||leftovers|pressure|bravebird,roost,bodypress,uturn|Impish|248,,168,,92,|||||,,,,,Flying',
	], [
		'Great Tusk||heavydutyboots|protosynthesis|headlongrush,closecombat,icespinner,rapidspin|Jolly|,252,,,4,252|||||,,,,,Ground',
		'Iron Valiant||leftovers|quarkdrive|moonblast,closecombat,thunderbolt,knockoff|Naive|,252,,4,,252|||||,,,,,Fairy',
		'Roaring Moon||heavydutyboots|protosynthesis|knockoff,earthquake,dragondance,acrobatics|Jolly|,252,,,4,252|||||,,,,,Flying',
		'Gholdengo||choicescarf|goodasgold|makeitrain,shadowball,thunderbolt,focusblast|Timid|,,,252,4,252|||||,,,,,Steel',
		'Kingambit||leftovers|supremeoverlord|kowtowcleave,suckerpunch,ironhead,swordsdance|Adamant|232,252,,,,24|||||,,,,,Dark',
		'Dragapult||choicespecs|clearbody|dragondarts,shadowball,dracometeor,flamethrower|Timid|,,,252,4,252|||||,,,,,Dragon',
	]],
	pivot: [[
		'Slowking-Galar||heavydutyboots|regenerator|chillyreception,sludgebomb,fireblast,psyshock|Calm|252,,16,,240,|||||,,,,,Dark',
		'Deoxys-Defense||leftovers|pressure|teleport,knockoff,psychicnoise,stealthrock|Bold|252,252,4,,,|||||,,,,,Steel',
		'Solgaleo||leftovers|fullmetalbody|teleport,sunsteelstrike,closecombat,flareblitz|Adamant|252,252,,,4,|||||,,,,,Steel',
		'Slowking||leftovers|regenerator|chillyreception,psychic,sludgebomb,slackoff|Bold|252,,252,,4,|||||,,,,,Water',
		'Blissey||leftovers|naturalcure|seismictoss,softboiled,icebeam,teleport|Calm|252,,252,,4,|||||,,,,,Normal',
		'Corviknight||leftovers|pressure|bravebird,roost,bodypress,uturn|Impish|248,,168,,92,|||||,,,,,Flying',
	], [
		'Great Tusk||heavydutyboots|protosynthesis|headlongrush,closecombat,icespinner,rapidspin|Jolly|,252,,,4,252|||||,,,,,Ground',
		'Iron Valiant||leftovers|quarkdrive|moonblast,closecombat,thunderbolt,knockoff|Naive|,252,,4,,252|||||,,,,,Fairy',
		'Roaring Moon||heavydutyboots|protosynthesis|knockoff,earthquake,dragondance,acrobatics|Jolly|,252,,,4,252|||||,,,,,Flying',
		'Gholdengo||choicescarf|goodasgold|makeitrain,shadowball,thunderbolt,focusblast|Timid|,,,252,4,252|||||,,,,,Steel',
		'Kingambit||leftovers|supremeoverlord|kowtowcleave,suckerpunch,ironhead,swordsdance|Adamant|232,252,,,,24|||||,,,,,Dark',
		'Dragapult||choicespecs|clearbody|dragondarts,shadowball,dracometeor,flamethrower|Timid|,,,252,4,252|||||,,,,,Dragon',
	]],
};

// State-machine move coverage (`c2*.json.gz`, `--teamset smA` / `smB`): the "state-machine"
// batch from coverage-worklist.json — Belly Drum, Clangorous Soul, Court Change, Meteor Beam
// (charge + Power-Herb skip), Future Sight, Endeavor, Mirror Coat, Beat Up, Double Shock, Explosion,
// Gigaton Hammer / Blood Moon (cantusetwice), and Booster Energy Quark Drive / Protosynthesis
// (consumed once, volatile cleared on switch). Carriers are the worklist's real gen9 carriers.
const TEAM_SM = {
	smA: [[
		'Azumarill||sitrusberry|hugepower|bellydrum,aquajet,playrough,liquidation|Adamant|,252,,,4,252|||||,,,,,Water',
		'Kommo-o||leftovers|bulletproof|clangoroussoul,drainpunch,closecombat,poisonjab|Adamant|,252,,,4,252|||||,,,,,Fighting',
		'Slowking||leftovers|regenerator|futuresight,scald,slackoff,sludgebomb|Calm|252,,16,,240,|||||,,,,,Water',
		'Cinderace||heavydutyboots|blaze|courtchange,pyroball,uturn,suckerpunch|Jolly|,252,,,4,252|||||,,,,,Fire',
		'Tinkaton||airballoon|moldbreaker|gigatonhammer,playrough,stealthrock,encore|Jolly|,252,,,4,252|||||,,,,,Steel',
		'Pawmot||lifeorb|ironfist|doubleshock,closecombat,machpunch,nuzzle|Jolly|,252,,,4,252|||||,,,,,Electric',
	], [
		'Glimmora||leftovers|toxicdebris|meteorbeam,powergem,earthpower,mortalspin|Modest|,,,252,4,252|||||,,,,,Rock',
		'Armarouge||powerherb|weakarmor|meteorbeam,armorcannon,psychic,aurasphere|Modest|,,,252,4,252|||||,,,,,Fire',
		'Ursaluna-Bloodmoon||leftovers|mindseye|bloodmoon,earthpower,hypervoice,calmmind|Modest|248,,,252,8,|||||,,,,,Normal',
		'Magnezone||leftovers|sturdy|mirrorcoat,thunderbolt,flashcannon,bodypress|Modest|252,,,252,4,|||||,,,,,Electric',
		'Fezandipiti||blackglasses|technician|beatup,gunkshot,uturn,roost|Jolly|,252,,,4,252|||||,,,,,Poison',
		'Regieleki||leftovers|transistor|explosion,thunderbolt,voltswitch,thunder|Timid|,,,252,4,252|||||,,,,,Electric',
	]],
	smB: [[
		'Iron Valiant||boosterenergy|quarkdrive|moonblast,closecombat,knockoff,thunderbolt|Naive|,252,,4,,252|||||,,,,,Fairy',
		'Great Tusk||boosterenergy|protosynthesis|headlongrush,closecombat,rapidspin,icespinner|Jolly|,252,,,4,252|||||,,,,,Ground',
		'Luvdisc||focussash|hydration|endeavor,whirlpool,protect,sweetkiss|Timid|,,,252,4,252|||||,,,,,Water',
		'Araquanid||leftovers|waterbubble|mirrorcoat,liquidation,leechlife,bugbuzz|Adamant|248,252,,,8,|||||,,,,,Water',
		'Iron Jugulis||boosterenergy|quarkdrive|meteorbeam,darkpulse,airslash,earthpower|Timid|,,,252,4,252|||||,,,,,Dark',
		'Electrode||leftovers|aftermath|explosion,thunderbolt,voltswitch,thunder|Timid|,,,252,4,252|||||,,,,,Electric',
	], [
		'Roaring Moon||boosterenergy|protosynthesis|knockoff,earthquake,dragondance,acrobatics|Jolly|,252,,,4,252|||||,,,,,Flying',
		'Raging Bolt||boosterenergy|protosynthesis|thunderbolt,dracometeor,thunderclap,calmmind|Modest|64,,,252,,192|||||,,,,,Electric',
		'Iron Hands||boosterenergy|quarkdrive|drainpunch,thunderpunch,swordsdance,closecombat|Adamant|,252,,,4,252|||||,,,,,Fighting',
		'Slowking-Galar||leftovers|regenerator|futuresight,sludgebomb,flamethrower,slackoff|Calm|252,,16,,240,|||||,,,,,Water',
		'Gholdengo||choicespecs|goodasgold|makeitrain,shadowball,thunderbolt,focusblast|Timid|,,,252,4,252|||||,,,,,Steel',
		'Kingambit||leftovers|supremeoverlord|kowtowcleave,suckerpunch,ironhead,swordsdance|Adamant|232,252,,,,24|||||,,,,,Dark',
	]],
};

// Final coverage tranche (`c3*.json.gz`, `--teamset c3a1`..): directed games whose movesets are
// dominated by the remaining unexercised randbats-eligible moves (harness/coverage-worklist.json
// carriers), so the harness's uniform choice policy exercises them quickly. Fillers are
// already-exercised moves; defensive anchors keep games long enough for full moveset coverage.
const TEAM_C3 = {
	// Mechanics: Fake Out first-turn-only, Bug Bite berry steal (Sitrus Dunsparce), Heal Bell
	// party cure, Switcheroo, Rage Fist, Rattled (+1 Spe on Bug/Dark/Ghost hit or Intimidate,
	// on its real trigger web: bugbite/knockoff/ragefist + Incineroar Intimidate), Payback,
	// Hex + status spreaders, Burning Jealousy vs the bulk-up/swords-dance side, dual-secondary
	// fangs, Supercell Slam crash, Ruination, Coil, Ice Hammer, Torch Song, Hypnosis (no sleep
	// clause in customgame — the engine keys the clause off the format).
	c3a1: [[
		'Incineroar||heavydutyboots|intimidate|fakeout,flareblitz,knockoff,uturn|Jolly|,252,,,4,252|||||,,,,,Fire',
		'Scizor||leftovers|technician|bugbite,bulletpunch,swordsdance,roost|Adamant|252,252,,,4,|||||,,,,,Bug',
		'Blissey||leftovers|naturalcure|healbell,seismictoss,softboiled,icebeam|Calm|252,,252,,4,|||||,,,,,Normal',
		'Annihilape||leftovers|defiant|ragefist,drainpunch,bulkup,taunt|Adamant|252,252,,,4,|||||,,,,,Fighting',
		'Persian||silkscarf|technician|switcheroo,fakeout,knockoff,doubleedge|Jolly|,252,,,4,252|||||,,,,,Normal',
		'Garchomp||leftovers|roughskin|firefang,earthquake,swordsdance,dragonclaw|Jolly|,252,,,4,252|||||,,,,,Ground',
	], [
		'Dunsparce||sitrusberry|rattled|coil,headbutt,earthquake,roost|Careful|252,,252,,4,|||||,,,,,Normal',
		'Ting-Lu||leftovers|vesselofruin|payback,ruination,earthquake,whirlwind|Careful|252,,4,,252,|||||,,,,,Dark',
		'Skeledirge||heavydutyboots|unaware|hex,torchsong,willowisp,slackoff|Bold|248,,252,,8,|||||,,,,,Fire',
		'Luxray||leftovers|intimidate|supercellslam,icefang,crunch,thunderwave|Adamant|252,252,,,4,|||||,,,,,Electric',
		'Crabominable||leftovers|ironfist|icehammer,closecombat,earthquake,bulletpunch|Adamant|252,252,,,4,|||||,,,,,Ice',
		'Persian-Alola||leftovers|furcoat|burningjealousy,darkpulse,nastyplot,hypnosis|Timid|,,,252,4,252|||||,,,,,Dark',
	]],
	// Mechanics: Truant Giga Impact (attack/recharge/loaf cycle), Skill Link Icicle Spear +
	// Drill Run, priority Accelerock, Bitter Blade drain, Blue Flare / Bolt Strike (the two
	// fusion counterparts stay in SEPARATE games so `lastSuccessfulMoveThisTurn` never
	// matters), Circle Throw phazing + Rain Dance, Berserk Fiery Wrath + Agility, Sheer Force
	// Esper Wing, Illusion Bitter Malice, Iron Tail, Poison Fang toxic.
	c3a2: [[
		'Lycanroc||lifeorb|sandrush|accelerock,closecombat,crunch,stoneedge|Jolly|,252,,,4,252|||||,,,,,Rock',
		'Ceruledge||heavydutyboots|weakarmor|bitterblade,shadowsneak,swordsdance,closecombat|Jolly|,252,,,4,252|||||,,,,,Fire',
		'Reshiram||heavydutyboots|turboblaze|blueflare,dracometeor,flamethrower,earthpower|Timid|,,,252,4,252|||||,,,,,Fire',
		'Poliwrath||leftovers|waterabsorb|circlethrow,raindance,waterfall,drainpunch|Adamant|252,252,,,4,|||||,,,,,Water',
		'Cloyster||leftovers|skilllink|iciclespear,drillrun,shellsmash,surf|Jolly|,252,,,4,252|||||,,,,,Ice',
		'Slaking||leftovers|truant|gigaimpact,doubleedge,earthquake,nightslash|Adamant|252,252,,,4,|||||,,,,,Normal',
	], [
		'Zekrom||leftovers|teravolt|boltstrike,outrage,dragondance,substitute|Adamant|252,252,,,4,|||||,,,,,Electric',
		'Zoroark-Hisui||heavydutyboots|illusion|bittermalice,shadowball,nastyplot,focusblast|Timid|,,,252,4,252|||||,,,,,Ghost',
		'Moltres-Galar||heavydutyboots|berserk|fierywrath,agility,hurricane,rest|Bold|248,,252,,8,|||||,,,,,Dark',
		'Braviary-Hisui||leftovers|sheerforce|esperwing,bravebird,closecombat,agility|Jolly|,252,,,4,252|||||,,,,,Psychic',
		'Orthworm||leftovers|eartheater|irontail,bodypress,stealthrock,rest|Impish|252,,252,,4,|||||,,,,,Steel',
		'Mightyena||leftovers|intimidate|poisonfang,crunch,suckerpunch,playrough|Adamant|252,252,,,4,|||||,,,,,Dark',
	]],
	// Mop-up: First Impression (first-turn-only legality — used later it FAILS, exercising the
	// activeMoveActions gate) and Poison Fang's 50% toxic, against poisonable tanks so games
	// run long enough for both.
	c3a3: [[
		'Lokix||heavydutyboots|swarm|firstimpression,uturn,suckerpunch,leechlife|Adamant|,252,,,4,252|||||,,,,,Bug',
		'Blissey||leftovers|naturalcure|seismictoss,softboiled,icebeam,protect|Calm|252,,252,,4,|||||,,,,,Normal',
	], [
		'Mightyena||leftovers|intimidate|poisonfang,crunch,suckerpunch,taunt|Adamant|252,252,,,4,|||||,,,,,Dark',
		'Milotic||leftovers|marvelscale|scald,recover,haze,icebeam|Bold|252,,252,,4,|||||,,,,,Water',
	]],
	// Forme + on-hit mechanics: Relic Song forme swap (Meloetta MUST run the randbats spread —
	// 85 EVs / 31 IVs / neutral nature — the engine's forme stat recompute assumes it), Aura
	// Wheel + Hunger Switch forme toggle, Shell Side Arm category pick, Hydro Steam boosted in
	// drought sun, Flash Fire Eruption, Stone Axe SR-on-hit, Spirit Shackle trapping, Unseen
	// Fist Wicked Blow vs Protect, Protean Flower Trick, Thermal Exchange Glaive Rush
	// Baxcalibur, Dynamax Cannon.
	c3b1: [[
		'Meloetta||leftovers|serenegrace|relicsong,psychic,closecombat,uturn|Serious|85,85,85,85,85,85|||||,,,,,Psychic',
		'Morpeko||leftovers|hungerswitch|aurawheel,crunch,rapidspin,protect|Jolly|,252,,,4,252|||||,,,,,Electric',
		'Slowbro-Galar||leftovers|regenerator|shellsidearm,slackoff,psychic,icebeam|Calm|252,,16,,240,|||||,,,,,Water',
		'Walking Wake||leftovers|protosynthesis|hydrosteam,dracometeor,flamethrower,knockoff|Timid|,,,252,4,252|||||,,,,,Water',
		'Torkoal||heatrock|drought|lavaplume,rapidspin,bodypress,yawn|Bold|248,,252,,8,|||||,,,,,Fire',
		'Typhlosion||heavydutyboots|flashfire|eruption,flamethrower,focusblast,shadowball|Timid|,,,252,4,252|||||,,,,,Fire',
	], [
		'Kleavor||heavydutyboots|sharpness|stoneaxe,xscissor,closecombat,uturn|Jolly|,252,,,4,252|||||,,,,,Bug',
		'Decidueye||heavydutyboots|overgrow|spiritshackle,leafblade,shadowsneak,roost|Adamant|252,252,,,4,|||||,,,,,Ghost',
		'Urshifu||leftovers|unseenfist|wickedblow,closecombat,suckerpunch,uturn|Jolly|,252,,,4,252|||||,,,,,Dark',
		'Meowscarada||heavydutyboots|protean|flowertrick,knockoff,uturn,playrough|Jolly|,252,,,4,252|||||,,,,,Grass',
		'Baxcalibur||heavydutyboots|thermalexchange|glaiverush,earthquake,iciclecrash,dragondance|Adamant|,252,,,4,252|||||,,,,,Dragon',
		'Eternatus||heavydutyboots|pressure|dynamaxcannon,flamethrower,sludgebomb,recover|Timid|,,,252,4,252|||||,,,,,Poison',
	]],
	// Setup/utility + stat mechanics: Multiscale Aeroblast, Moxie Aqua Step, Clanging Scales,
	// Psychic Surge Expanding Force (terrain boost + priority block vs Vacuum Wave / Sucker
	// Punch), Behemoth Blade, Sap Sipper Milk Drink Gogoat vs Matcha Gotcha (absorb + drain +
	// thaw move), Heatproof Sinistcha, Soul Dew Luster Purge, Justified Meteor Mash + Vacuum
	// Wave Lucario, Hustle Grav Apple, Quark Drive Mighty Cleave, Fusion Flare (its Bolt
	// counterpart lives in a different game so `lastSuccessfulMoveThisTurn` never matters).
	c3b2: [[
		'Lugia||leftovers|multiscale|aeroblast,calmmind,recover,psyshock|Timid|,,,252,4,252|||||,,,,,Psychic',
		'Quaquaval||leftovers|moxie|aquastep,closecombat,rapidspin,roost|Jolly|,252,,,4,252|||||,,,,,Water',
		'Kommo-o||leftovers|bulletproof|clangingscales,drainpunch,closecombat,poisonjab|Adamant|,252,,,4,252|||||,,,,,Fighting',
		'Indeedee||leftovers|psychicsurge|expandingforce,hypervoice,calmmind,healingwish|Timid|,,,252,4,252|||||,,,,,Psychic',
		'Zacian-Crowned||rustedsword|intrepidsword|behemothblade,playrough,closecombat,swordsdance|Jolly|,252,,,4,252|||||,,,,,Steel',
		'Gogoat||leftovers|sapsipper|milkdrink,hornleech,earthquake,bulkup|Adamant|248,252,,,8,|||||,,,,,Grass',
	], [
		'Kyurem-White||heavydutyboots|turboblaze|fusionflare,dracometeor,icebeam,focusblast|Timid|,,,252,4,252|||||,,,,,Ice',
		'Iron Boulder||boosterenergy|quarkdrive|mightycleave,closecombat,zenheadbutt,swordsdance|Jolly|,252,,,4,252|||||,,,,,Rock',
		'Sinistcha||leftovers|heatproof|matchagotcha,shadowball,calmmind,strengthsap|Bold|248,,252,,8,|||||,,,,,Grass',
		'Latios||souldew|levitate|lusterpurge,dracometeor,recover,calmmind|Timid|,,,252,4,252|||||,,,,,Dragon',
		'Lucario||lifeorb|justified|meteormash,vacuumwave,closecombat,swordsdance|Jolly|,252,,,4,252|||||,,,,,Fighting',
		'Flapple||leftovers|hustle|gravapple,outrage,suckerpunch,dragondance|Jolly|,252,,,4,252|||||,,,,,Dragon',
	]],
	// Mop-up 2v2: Kleavor's Stone Axe (SR on hit) and Indeedee's Expanding Force (Psychic
	// Surge terrain ×1.5) as leads with guaranteed field time — both drowned on the bench in
	// the full c3b games.
	c3b3: [[
		'Kleavor||heavydutyboots|sharpness|stoneaxe,xscissor,closecombat,quickattack|Jolly|,252,,,4,252|||||,,,,,Bug',
		'Blissey||leftovers|naturalcure|seismictoss,softboiled,icebeam,protect|Calm|252,,252,,4,|||||,,,,,Normal',
	], [
		'Indeedee||leftovers|psychicsurge|expandingforce,hypervoice,calmmind,protect|Timid|,,,252,4,252|||||,,,,,Psychic',
		'Corviknight||leftovers|pressure|bravebird,roost,bodypress,irondefense|Impish|248,,168,,92,|||||,,,,,Flying',
	]],
};

// Ability coverage (`d*.json.gz`, `--teamset flashfire`): a Flash Fire carrier (Heatran) faces
// Fire attackers so the ability's immunity + `flashfire` activation volatile fires, and the
// carrier's own Fire moves then get the ×1.5 boost. Includes a Fire status move (Will-O-Wisp)
// to exercise the status-move immunity path too.
const TEAM_DIRECTED = {
	flashfire: [[
		'Heatran||leftovers|flashfire|magmastorm,lavaplume,earthpower,flashcannon|Modest|252,,,252,4,|||||,,,,,Fire',
		'Amoonguss||leftovers|regenerator|gigadrain,sludgebomb,clearsmog,synthesis|Bold|252,,252,,4,|||||,,,,,Grass',
	], [
		'Arcanine||heavydutyboots|justified|flareblitz,willowisp,extremespeed,morningsun|Jolly|,252,,,4,252|||||,,,,,Fire',
		'Volcarona||heavydutyboots|swarm|flamethrower,fierydance,bugbuzz,roost|Timid|,,,252,4,252|||||,,,,,Bug',
	]],
	// U-turn with a fully-fainted bench (the frail lead faints, then the survivor U-turns and
	// stays), while the foe carries Scald and a Rage Fist counter — probes `times_hit` accounting.
	uturnbench: [[
		'Magikarp||focussash|rattled|splash,tackle,flail,bounce|Jolly|,252,,,4,252|||||,,,,,Water',
		'Scizor||choiceband|technician|uturn,bulletpunch,closecombat,knockoff|Adamant|,252,,,4,252|||||,,,,,Bug',
	], [
		'Annihilape||leftovers|defiant|ragefist,uturn,drainpunch,bulkup|Adamant|,252,,,4,252|||||,,,,,Fighting',
		'Toxapex||blacksludge|regenerator|scald,recover,toxic,haze|Bold|252,,252,,4,|||||,,,,,Poison',
	]],
};

// ---- deterministic choice RNG -------------------------------------------------

function makeRng(seed) {
	let s = BigInt(seed) + 0x9e3779b97f4a7c15n;
	return (n) => {
		s = (s + 0x9e3779b97f4a7c15n) & 0xffffffffffffffffn;
		let z = s;
		z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & 0xffffffffffffffffn;
		z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & 0xffffffffffffffffn;
		z = z ^ (z >> 31n);
		return Number(z % BigInt(n));
	};
}

// ---- PRNG instrumentation: the shared outcome alphabet -------------------------

function instrumentPrng(battle, draws) {
	const prng = battle.prng;
	const label = () => ({
		event: battle.event?.id ?? null,
		effect: battle.effect?.id ?? null,
		move: battle.activeMove?.id ?? null,
		pokemon: battle.activePokemon?.fullname ?? null,
	});
	// randomChance/sample call this.random internally; record only the outermost call so each
	// semantic draw appears exactly once.
	let depth = 0;
	const wrap = (name, fmtArgs) => {
		const orig = prng[name].bind(prng);
		prng[name] = (...args) => {
			depth++;
			let result;
			try {
				result = orig(...args);
			} finally {
				depth--;
			}
			if (depth === 0) {
				draws.push({ kind: name, args: fmtArgs(args), result: serializeResult(name, result, args), ...label() });
			}
			return result;
		};
	};
	wrap('random', (a) => a.filter(x => x !== undefined));
	wrap('randomChance', (a) => a);
	wrap('sample', (a) => [a[0]?.length]);          // record domain size, result index below
	wrap('shuffle', (a) => [a[0]?.length, a[1], a[2]]);
}

function serializeResult(kind, result, args) {
	if (kind === 'sample') {
		// store the index into the sampled list so the result is stable/serializable
		const idx = args[0].indexOf(result);
		return { index: idx, value: stringify(result) };
	}
	if (kind === 'shuffle') return null; // mutation in place; order divergence shows in outcomes
	return result;
}

function stringify(v) {
	if (v == null) return null;
	if (typeof v === 'object') return v.fullname ?? v.id ?? v.name ?? String(v);
	return v;
}

// ---- state snapshot (full serializeBattle, minus noise) -------------------------

const DROP_TOP = new Set([
	'log', 'messageLog', 'inputLog', 'sentRequests', 'hints', 'queue', 'events', 'eventDepth',
	'effect', 'effectState', 'event', 'formatData', 'sentLogPos', 'sentEnd', 'activeMove',
	'activePokemon', 'activeTarget', 'lastMoveLine', 'prng', 'prngSeed', 'speedOrder',
	'lastSuccessfulMoveThisTurn', 'lastDamage', 'effectOrder', 'quickClawRoll', 'formatid',
	'debugMode', 'forceRandomChance', 'strictChoices', 'gameType', 'activePerHalf', 'rated',
	'reportExactHP', 'reportPercentages', 'supportCancel', 'faintQueue', 'requestState', 'started',
]);
const DROP_SIDE = new Set(['foe', 'allySide', 'team', 'choice', 'avatar', 'activeRequest']);
const DROP_POKEMON = new Set(['m', 'set', 'speciesState', 'moveLastTurnResult']);

function setKey(set) {
	return JSON.stringify(set);
}

function snapshot(battle, roster) {
	const ser = State.serializeBattle(battle);
	const out = {};
	for (const k of Object.keys(ser)) {
		if (!DROP_TOP.has(k)) out[k] = ser[k];
	}
	out.sides = ser.sides.map((side, si) => {
		const s = {};
		for (const k of Object.keys(side)) {
			if (!DROP_SIDE.has(k)) s[k] = side[k];
		}
		s.pokemon = side.pokemon.map((p, pi) => {
			const q = {};
			for (const k of Object.keys(p)) {
				if (!DROP_POKEMON.has(k)) q[k] = p[k];
			}
			// Stable identity across PS's array reordering AND forme changes (Palafin-Hero
			// changes `details` mid-battle): index into the battle-start roster, keyed by the
			// live pokemon's immutable `set` object.
			// Deserialized distribution branches contain cloned set objects, so object identity
			// is unavailable. Sets are immutable during battle; structural identity preserves the
			// battle-start roster index across clones and forme changes.
			const key = setKey(battle.sides[si].pokemon[pi].set);
			q.rosterIndex = roster[si].findIndex(s => setKey(s) === key);
			return q;
		});
		// Synthesized: has this side spent its Terastallization? PS deletes `terastallized`
		// from a mon when it faints (battle.ts `delete pokemon.terastallized`), so presence of
		// a tera'd mon is NOT a reliable signal. PS's own source of truth is per-mon
		// `canTerastallize`, nulled for the whole team when any ally teras.
		s.teraUsed = !battle.sides[si].pokemon.some(p => !!p.canTerastallize);
		return s;
	});
	out.winner = battle.winner ?? null;
	return JSON.parse(JSON.stringify(out)); // deep-copy + drop undefineds
}

// ---- exact decision-point distribution oracle ---------------------------------

class NeedRandom extends Error {
	constructor(kind, args, options) {
		super(`need random outcome ${kind}`);
		this.kind = kind;
		this.args = args;
		this.options = options;
	}
}

function installForcedPrng(battle, prefix) {
	let cursor = 0;
	const take = (kind, args, options) => {
		if (cursor < prefix.length) {
			const forced = prefix[cursor++];
			if (forced.kind !== kind || JSON.stringify(forced.args) !== JSON.stringify(args)) {
				throw new Error(`RNG prefix drift at ${cursor - 1}: expected ${forced.kind}${JSON.stringify(forced.args)}, got ${kind}${JSON.stringify(args)}`);
			}
			return forced.value;
		}
		throw new NeedRandom(kind, args, options);
	};
	const prng = battle.prng;
	prng.random = (from, to) => {
		let lo, hi;
		if (from === undefined) {
			// No current battle mechanic in the corpus uses random() as a float. Keep the
			// unsupported case explicit instead of pretending a continuous distribution is finite.
			throw new Error('distribution oracle does not support zero-argument random()');
		} else if (to === undefined || !to) {
			lo = 0; hi = Math.floor(from);
		} else {
			lo = Math.floor(from); hi = Math.floor(to);
		}
		const n = hi - lo;
		return take('random', [lo, hi], Array.from({ length: n }, (_, i) => ({ value: lo + i, probability: 1 / n })));
	};
	prng.randomChance = (numerator, denominator) => {
		// PS semantics: randomChance(n, d) is random(d) < n, so n >= d (e.g. boosted
		// accuracy 130/100) is certainty. Clamp before turning the draw into branch
		// probabilities or the enumeration emits mass > 1 and trips the mass check.
		const clamped = Math.min(Math.max(numerator, 0), denominator);
		return take('randomChance', [numerator, denominator], [
			{ value: true, probability: clamped / denominator },
			{ value: false, probability: (denominator - clamped) / denominator },
		].filter(x => x.probability > 0));
	};
	prng.sample = items => {
		if (!items.length) throw new RangeError('Cannot sample an empty array');
		const index = take('sample', [items.length], items.map((_, i) => ({ value: i, probability: 1 / items.length })));
		return items[index];
	};
	// Keep PS's shuffle implementation: it calls the forced `random(start, end)` above for
	// each Fisher-Yates step, yielding the exact permutation distribution.
}

class ActionComplete extends Error {
	constructor(action, inputState) {
		super(`action complete: ${action.choice}`);
		this.action = action;
		this.inputState = inputState;
	}
}

function summarizeAction(action, battle) {
	const foePending = action.pokemon ? battle.queue.list.find(a =>
		a.choice === 'move' && a.pokemon?.side !== action.pokemon.side
	) : null;
	return {
		choice: action.choice,
		side: action.pokemon?.side?.id ?? null,
		moveId: action.move?.id ?? null,
		foePendingMoveId: foePending?.move?.id ?? null,
	};
}

function cleanCheckpoint(state) {
	const checkpoint = structuredClone(state);
	checkpoint.log = [];
	checkpoint.messageLog = [];
	checkpoint.sentLogPos = 0;
	return checkpoint;
}

function checkpointJson(state) {
	return JSON.stringify(cleanCheckpoint(state));
}

function enumerateDecisionDistribution(checkpoint, choices, roster) {
	// Expand one queued Showdown action at a time and coalesce identical serialized states
	// between actions. This computes the same exact distribution as whole-turn prefix replay,
	// but avoids multiplying independent damage/crit/secondary paths across both moves.
	let replayLeaves = 0;
	// A serialized battle is a large, deeply nested object. Keeping one expanded object per
	// stochastic state makes V8 spend several times the JSON payload size on object metadata.
	// Store the restartable frontier in its wire representation and parse only the state being
	// replayed. The same string is also its exact coalescing key, avoiding a second full copy.
	let frontier = [{ checkpoint: checkpointJson(checkpoint), probability: 1, initial: true }];
	const outcomes = new Map();
	const kernels = [];

	while (frontier.length) {
		const nextStages = new Map();
		for (const stage of frontier) {
			const work = [{ prefix: [], probability: stage.probability }];
			const kernelGroups = new Map();
			while (work.length) {
				const job = work.pop();
				const branch = State.deserializeBattle(JSON.parse(stage.checkpoint));
				branch.restart(() => {});
				installForcedPrng(branch, job.prefix);
				const originalRunAction = branch.runAction.bind(branch);
				branch.runAction = action => {
					const inputState = snapshot(branch, roster);
					originalRunAction(action);
					throw new ActionComplete(summarizeAction(action, branch), inputState);
				};
				let paused = false;
				let completedAction = null;
				try {
					if (stage.initial) {
						for (const [sideId, c] of Object.entries(choices)) {
							const ok = branch.choose(sideId, c.choice);
							if (!ok) throw new Error(`PS rejected distribution choice '${c.choice}' for ${sideId}`);
						}
					} else {
						branch.turnLoop();
					}
				} catch (e) {
					if (e instanceof NeedRandom) {
						for (const option of e.options) {
							work.push({
								prefix: [...job.prefix, { kind: e.kind, args: e.args, value: option.value }],
								probability: job.probability * option.probability,
							});
						}
						continue;
					}
					if (!(e instanceof ActionComplete)) throw e;
					paused = true;
					completedAction = e;
				}

				replayLeaves++;
				if (replayLeaves > MAX_DIST_PATHS) throw new Error(`distribution path cap ${MAX_DIST_PATHS} exceeded`);
				// A request/terminal boundary is an observable endpoint even if it arose within an
				// action. Otherwise serialize the remaining queue and resume at the next action.
				if (branch.requestState || branch.ended || !paused) {
					const state = snapshot(branch, roster);
					const requestState = branch.requestState;
					const midTurn = !!branch.midTurn;
					const key = JSON.stringify({ requestState, midTurn, state });
					outcomes.set(key, (outcomes.get(key) ?? 0) + job.probability);
				} else {
					const next = checkpointJson(State.serializeBattle(branch));
					const key = next;
					const prev = nextStages.get(key);
					if (prev) prev.probability += job.probability;
					else nextStages.set(key, { checkpoint: next, probability: job.probability, initial: false });
				}
				// Rust currently certifies stochastic move kernels. Retaining kernels for
				// residual/beforeTurn/tera actions is pure duplication: a residual stage can
				// have thousands of distinct inputs, and every entry embeds two full battle
				// snapshots even though the consumer immediately filters it out. Continue
				// staging every action (and include its effects in terminal outcomes), but
				// materialize only the move kernels that are actually compared.
				if (completedAction?.action.choice === 'move') {
					const groupKey = JSON.stringify({ action: completedAction.action, input: completedAction.inputState });
					let group = kernelGroups.get(groupKey);
					if (!group) {
						group = { action: completedAction.action, input: completedAction.inputState, outcomes: new Map(), mass: 0 };
						kernelGroups.set(groupKey, group);
					}
					const output = snapshot(branch, roster);
					const key = JSON.stringify(output);
					const conditional = job.probability / stage.probability;
					group.mass += conditional;
					group.outcomes.set(key, (group.outcomes.get(key) ?? 0) + conditional);
				}
			}
			for (const group of kernelGroups.values()) {
				kernels.push({
					action: group.action,
					input: group.input,
					selectionProbability: group.mass,
					outcomes: [...group.outcomes].map(([state, probability]) => ({
						probability: probability / group.mass,
						state: JSON.parse(state),
					})),
				});
			}
		}
		frontier = [...nextStages.values()];
	}

	const distribution = [...outcomes].map(([outcome, probability]) => ({
		probability,
		...JSON.parse(outcome),
	})).sort((a, b) => JSON.stringify(a.state).localeCompare(JSON.stringify(b.state)));
	const total = distribution.reduce((s, x) => s + x.probability, 0);
	if (Math.abs(total - 1) > 1e-10) throw new Error(`distribution mass is ${total}, expected 1`);
	return { paths: replayLeaves, outcomes: distribution, kernels };
}

// ---- choice policy ---------------------------------------------------------------

// Uniform-ish over legal options: mostly moves, sometimes a voluntary switch (the old
// harness never made voluntary switches — a coverage hole), tera ~25% when available.
function chooseFor(request, rand) {
	if (!request || request.wait) return null;
	if (request.teamPreview) return { choice: 'default', resolved: { action: 'teampreview' } };

	if (request.forceSwitch) {
		const mons = request.side.pokemon;
		// Revival Blessing raises a switch request whose target must be a FAINTED party member
		// (PS marks the active entry `reviving: true`); a normal forced switch wants a live mon.
		const reviving = mons.some(m => m.reviving);
		const isFainted = (m) => m.condition.endsWith(' fnt') || m.condition === '0 fnt';
		const options = [];
		for (let i = 0; i < mons.length; i++) {
			if (mons[i].active) continue;
			if (reviving ? isFainted(mons[i]) : !isFainted(mons[i])) options.push(i);
		}
		if (!options.length) return { choice: 'pass', resolved: { action: 'pass' } };
		const k = options[rand(options.length)];
		return {
			choice: `switch ${k + 1}`,
			resolved: { action: 'switch', ident: mons[k].ident, details: mons[k].details },
		};
	}

	if (request.active) {
		const act = request.active[0];
		const moves = [];
		for (let i = 0; i < act.moves.length; i++) {
			const m = act.moves[i];
			if (!m.disabled && (m.pp === undefined || m.pp > 0)) moves.push({ slot: i + 1, id: m.id });
		}
		const switches = [];
		// In singles PS reports volatile/locked traps as `trapped` but ability traps (Arena Trap /
		// Shadow Tag / Magnet Pull, discovered via tryTrap(true)) as `maybeTrapped` — attempting a
		// switch under either gets the choice rejected (pokemon.trapped is truthy).
		if (!act.trapped && !act.maybeTrapped) {
			const mons = request.side.pokemon;
			for (let i = 0; i < mons.length; i++) {
				if (!mons[i].active && !mons[i].condition.endsWith(' fnt') && mons[i].condition !== '0 fnt') {
					switches.push({ slot: i + 1, ident: mons[i].ident, details: mons[i].details });
				}
			}
		}
		// 15% voluntary switch when possible.
		if (switches.length && (!moves.length || rand(100) < 15)) {
			const s = switches[rand(switches.length)];
			return { choice: `switch ${s.slot}`, resolved: { action: 'switch', ident: s.ident, details: s.details } };
		}
		if (!moves.length) {
			// only Struggle remains; PS lists it as the lone move
			return { choice: 'move 1', resolved: { action: 'move', moveId: act.moves[0]?.id ?? 'struggle', tera: false } };
		}
		const mv = moves[rand(moves.length)];
		const tera = !!act.canTerastallize && rand(4) === 0;
		return {
			choice: `move ${mv.slot}${tera ? ' terastallize' : ''}`,
			resolved: { action: 'move', moveId: mv.id, tera },
		};
	}
	return { choice: 'default', resolved: { action: 'default' } };
}

// ---- main -----------------------------------------------------------------------

async function main() {
	// Teams: fixed packed sets, or PS-generated random-battle teams (deterministic per seed).
	let team1, team2;
	if (FORMAT.includes('random')) {
		team1 = Teams.pack(Teams.generate(FORMAT, { seed: [0, 0, 0, SEED_NUM * 2 + 1] }));
		team2 = Teams.pack(Teams.generate(FORMAT, { seed: [0, 0, 0, SEED_NUM * 2 + 2] }));
	} else {
		const [t1, t2] = TEAM_TRAP[TEAMSET]
			? TEAM_TRAP[TEAMSET].map(t => t.join(']'))
			: TEAM_C3[TEAMSET]
			? TEAM_C3[TEAMSET].map(t => t.join(']'))
			: TEAM_COVERAGE[TEAMSET]
			? TEAM_COVERAGE[TEAMSET].map(t => t.join(']'))
			: TEAM_SM[TEAMSET]
			? TEAM_SM[TEAMSET].map(t => t.join(']'))
			: TEAM_DIRECTED[TEAMSET]
			? TEAM_DIRECTED[TEAMSET].map(t => t.join(']'))
			: TEAMSET === 'diverse' ? [TEAM_DIVERSE_1, TEAM_DIVERSE_2] : [TEAM_OU_1, TEAM_OU_2];
		team1 = Teams.pack(Teams.import(t1));
		team2 = Teams.pack(Teams.import(t2));
	}
	const battle = new Battle({
		// Random-battle TEAMS are pre-generated above; the battle itself runs as a custom game
		// so PS doesn't re-roll teams from the battle seed.
		formatid: FORMAT.includes('random') ? 'gen9customgame' : FORMAT,
		seed: [SEED_NUM, SEED_NUM + 7, SEED_NUM + 13, SEED_NUM + 29],
		p1: { name: 'Red', team: team1 },
		p2: { name: 'Blue', team: team2 },
	});

	const roster = battle.sides.map(side => side.pokemon.map(p => p.set));
	const draws = [];
	instrumentPrng(battle, draws);
	const rng = { p1: makeRng(SEED_NUM * 2 + 1), p2: makeRng(SEED_NUM * 2 + 2) };

	const decisions = [];
	let guard = 0;
	while (!battle.ended && guard++ < MAX_DECISIONS) {
		const requestState = battle.requestState;
		const requests = {};
		const choices = {};
		for (const side of [battle.p1, battle.p2]) {
			const req = side.activeRequest;
			if (!req || req.wait) continue;
			requests[side.id] = JSON.parse(JSON.stringify(req));
			const c = chooseFor(req, rng[side.id]);
			if (c) {
				// Switch choices: also resolve to the stable roster index (forme-proof identity).
				const m = c.choice.match(/^switch (\d+)$/);
				if (m) {
					const target = side.pokemon[Number(m[1]) - 1];
					c.resolved.rosterIndex = roster[side.n].indexOf(target.set);
				}
				choices[side.id] = c;
			}
		}
		if (!Object.keys(choices).length) {
			throw new Error(`no side can act at decision ${decisions.length} (requestState=${requestState})`);
		}
		// Team-preview queue actions temporarily assemble side.pokemon and are not restartable
		// one action at a time through State.deserializeBattle. They are deterministic setup, not
		// part of the battle transition kernel we need to certify.
		const enumerate = DISTRIBUTIONS && requestState !== 'teampreview';
		const checkpoint = enumerate ? State.serializeBattle(battle) : null;
		const distribution = enumerate ? enumerateDecisionDistribution(checkpoint, choices, roster) : undefined;
		// Submitting the final needed choice runs the battle forward synchronously.
		for (const [sideId, c] of Object.entries(choices)) {
			const ok = battle.choose(sideId, c.choice);
			if (!ok) {
				throw new Error(`PS rejected choice '${c.choice}' for ${sideId} at decision ${decisions.length}: ${battle.inputLog.slice(-3).join(' | ')}`);
			}
		}
		decisions.push({
			index: decisions.length,
			turn: battle.turn,
			requestState,
			midTurn: !!battle.midTurn,
			requests,
			choices,
			draws: draws.splice(0, draws.length),
			stateAfter: snapshot(battle, roster),
			...(distribution ? { distribution } : {}),
		});
	}

	const trace = {
		version: 2,
		psCommit: PS_COMMIT,
		format: FORMAT,
		seed: [SEED_NUM, SEED_NUM + 7, SEED_NUM + 13, SEED_NUM + 29],
		teamset: TEAMSET,
		decisions,
		result: { winner: battle.winner ?? null, ended: battle.ended, turns: battle.turn },
	};
	fs.mkdirSync(path.dirname(OUT), { recursive: true });
	const body = JSON.stringify(trace);
	if (OUT.endsWith('.gz')) {
		const { gzipSync } = await import('node:zlib');
		fs.writeFileSync(OUT, gzipSync(body));
	} else {
		fs.writeFileSync(OUT, body);
	}
	console.log(`wrote ${OUT}: ${decisions.length} decisions, ${trace.result.turns} turns, winner=${trace.result.winner}, ps=${PS_COMMIT.slice(0, 12)}`);
}

main();
