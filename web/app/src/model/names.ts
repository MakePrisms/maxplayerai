/**
 * Generated display names for participants with no kind-0 name.
 *
 * Same style as the retired Try it generator (5f112e3f^, src/try/wire.ts):
 * `model-character`, `model-designation` or `model-character-designation`,
 * lowercase and hyphen-joined. Unlike that generator, nothing here is random:
 * every choice comes from SHA-256 of the 32-byte public key, so one key gets
 * one name on every page, device and reload.
 *
 * FROZEN once live. Adding, removing or reordering a word renames every
 * generated buyer on the site (the indices below are taken modulo the list
 * lengths). `test/buyer-names.test.ts` pins the lists for that reason.
 *
 * A real kind-0 `name` / `display_name` always wins; this is only the fallback.
 * Pure, no DOM: imported by the market board and usable from the swarm model.
 */
import { sha256 } from "@noble/hashes/sha256";

export const models: readonly string[] = Object.freeze((
  // The original 40 Try it models.
  "vantor stradale apex aerion velora radian corsair solaro torven caldera virelli ignis chicane slipstream redline camber aero apexion valtor revora ventaro torqen astera voltane cravon serran veyra falcar valden aleron zenora meridan orvex savaro draeven varion thalor kestrel pinion kerbline " +
  // Added 2026-10-31: invented model-like words.
  "alcora brivane castora delvara essaro fervana galvora halvane isoren javenta karvelo lumaro marveno nevaro oltrano pelaro quorvan rivanta sarvoni tavrello ulvera vescora wendaro xeloran yveron zarvento brennaro cortavo duvessa elvanto fioressa garreno loravex montavo novessa perlano rastello silvaro trevanto veltaro"
).split(" "));

export const characters: readonly string[] = Object.freeze((
  // The original 41 Try it characters.
  "nero veloce corsa comet sprint rosso tempest vector falcon spectre foudre strada fulmine rapida nocturne ember thunder lightning carbon titanium silver crimson scarlet obsidian onyx cobalt graphite jet sonic swift rapid fierce blazing charged nimble dusk dawn stealth storm fire frost " +
  // Added 2026-10-31: colours, materials, speed/weather, Italian/French words.
  "azzurro bianco giallo verde argento noir rouge bleu blanc amber ivory bronze copper sapphire chrome alloy granite quartz platinum gale squall monsoon downforce breeze hailstorm lampo notte alba eclair orage fougue ardente audace feroce nebbia glace soleil turbo velocita"
).split(" "));

export const designations: readonly string[] = Object.freeze("gt gtr rs gts rr".split(" "));

const HEX32 = /^[0-9a-f]{64}$/;

function keyBytes(hex: string): Uint8Array {
  const out = new Uint8Array(32);
  for (let i = 0; i < 32; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/**
 * The generated name for a 64-hex public key, or null when the input is not
 * one (never throws). Case-insensitive: a key and its uppercase spelling name
 * the same participant.
 *
 * h = SHA-256(key bytes). Form = h[0] mod 3; model = h[1..2] big-endian mod
 * |models|; character = h[3..4] big-endian mod |characters|; designation =
 * h[5] mod |designations|.
 */
export function nameForPubkey(hex: unknown): string | null {
  if (typeof hex !== "string") return null;
  const key = hex.toLowerCase();
  if (!HEX32.test(key)) return null;
  const h = sha256(keyBytes(key));
  const form = h[0]! % 3; // 0: model-character, 1: model-designation, 2: all three
  const model = models[((h[1]! << 8) | h[2]!) % models.length]!;
  const character = characters[((h[3]! << 8) | h[4]!) % characters.length]!;
  const designation = designations[h[5]! % designations.length]!;
  if (form === 0) return `${model}-${character}`;
  if (form === 1) return `${model}-${designation}`;
  return `${model}-${character}-${designation}`;
}

/** True for a name this module can produce (the Try it `validName` shape). */
export function validName(name: string): boolean {
  const p = name.split("-");
  return (
    models.includes(p[0]!) &&
    ((p.length === 2 && (characters.includes(p[1]!) || designations.includes(p[1]!))) ||
      (p.length === 3 && characters.includes(p[1]!) && designations.includes(p[2]!)))
  );
}

/**
 * The name to show: the kind-0 name when there is one, else the generated
 * name for a buyer, else null (the caller shows the key, as before). Sellers
 * are not given generated names.
 */
export function displayName(kind0: string | null | undefined, pubkey: string, buyer: boolean): string | null {
  return kind0 || (buyer ? nameForPubkey(pubkey) : null);
}

/** How many distinct names exist: M·C + M·D + M·C·D. */
export const NAME_SPACE = models.length * characters.length
  + models.length * designations.length
  + models.length * characters.length * designations.length;
