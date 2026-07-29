/**
 * Big-integer-safe JSON serialization for the cheatcodes transport.
 *
 * Kit's default transport only applies its bigint-aware JSON codec to the
 * standard Solana RPC methods (it allowlists them by name), so `surfnet_*`
 * responses would otherwise go through plain `JSON.parse` and silently lose
 * precision on u64 values above 2^53 — e.g. the `rentEpoch: u64::MAX`
 * rent-exempt marker present on most mainnet accounts — and `JSON.stringify`
 * would throw on bigint request parameters.
 *
 * Ported from `@solana/rpc-spec-types` (MIT). Kit does not re-export these
 * helpers, and depending on that internal Kit package directly would either
 * add a hard dependency tree to every plain `@solana/surfpool` install or, as
 * an optional peer, break under package managers that do not auto-install
 * peers (pnpm) — so the ~100 lines are vendored instead.
 */

const BIGINT_VALUE_OBJECT_PATTERN = /^(-?)(\d+)(?:[eE]\+?(\d+))?$/;
const MAX_BIGINT_DIGITS = 1000;

type BigIntValueObject = {
    /** A string containing the bigint's value, e.g. `{ $n: "9007199254740993" }`. */
    $n: string;
};

/**
 * Parses a JSON string, deserializing every integer literal (no decimal point,
 * no negative exponent) as a `bigint` to avoid precision loss above 2^53.
 */
export function parseJsonWithBigInts(json: string): unknown {
    return JSON.parse(wrapIntegersInBigIntValueObject(json), (_, value) => {
        return isBigIntValueObject(value) ? unwrapBigIntValueObject(value) : value;
    });
}

/**
 * Stringifies a value to JSON, serializing `bigint` values as plain JSON
 * integer literals.
 */
export function stringifyJsonWithBigInts(value: unknown): string {
    return unwrapBigIntValueObjectsInJson(
        JSON.stringify(value, (_, v) => (typeof v === 'bigint' ? { $n: `${v}` } : v)),
    );
}

function wrapIntegersInBigIntValueObject(json: string): string {
    const out: string[] = [];
    let inQuote = false;
    for (let ii = 0; ii < json.length; ii++) {
        let isEscaped = false;
        if (json[ii] === '\\') {
            out.push(json[ii++]);
            isEscaped = !isEscaped;
        }
        if (json[ii] === '"') {
            out.push(json[ii]);
            if (!isEscaped) {
                inQuote = !inQuote;
            }
            continue;
        }
        if (!inQuote) {
            const consumedNumber = consumeNumber(json, ii);
            if (consumedNumber?.length) {
                ii += consumedNumber.length - 1;
                if (consumedNumber.match(/\.|[eE]-/)) {
                    out.push(consumedNumber);
                } else {
                    out.push(`{"$n":"${consumedNumber}"}`);
                }
                continue;
            }
        }
        out.push(json[ii]);
    }
    return out.join('');
}

function consumeNumber(json: string, ii: number): string | null {
    const JSON_NUMBER_REGEX = /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/;
    if (!json[ii]?.match(/[-\d]/)) {
        return null;
    }
    const numberMatch = json.slice(ii).match(JSON_NUMBER_REGEX);
    return numberMatch ? numberMatch[0] : null;
}

function unwrapBigIntValueObject({ $n }: BigIntValueObject): bigint {
    const match = $n.match(BIGINT_VALUE_OBJECT_PATTERN);
    if (match) {
        const [, sign, mantissa, exponent] = match;
        const digitCount = mantissa.length + (exponent ? Number(exponent) : 0);
        if (digitCount <= MAX_BIGINT_DIGITS) {
            return exponent ? BigInt(`${sign}${mantissa}`) * 10n ** BigInt(exponent) : BigInt($n);
        }
    }
    throw new Error(`Malformed integer in Surfnet RPC response: ${$n}`);
}

function isBigIntValueObject(value: unknown): value is BigIntValueObject {
    return !!value && typeof value === 'object' && '$n' in value && typeof value.$n === 'string';
}

function unwrapBigIntValueObjectsInJson(json: string): string {
    return json.replace(/\{\s*"\$n"\s*:\s*"(-?\d+)"\s*\}/g, '$1');
}
