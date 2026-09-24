import { useContext } from 'react';
import { FieldContext } from './FieldContext';

/**
 * What a saved connection will supply for `fieldKey` at run time, for display only.
 *
 * A node that names a `connectionRef` deliberately does NOT carry these values:
 * `merge_generic_connection` fills them when the pipeline runs, and the connection
 * wins over anything inline, so copying them onto the node is the duplication the
 * saved connection exists to remove. The cost was that the editor showed the
 * manifest's placeholder instead - a Postgres node pointing at a connection on port
 * 15432 displayed the default 5432, which reads as wrong even though the run is
 * right.
 *
 * This returns the connection's value so the field can SHOW it without storing it.
 * Nothing here is ever written back to the node.
 */
export function useConnectionSupplied(fieldKey: string): string | undefined {
    const { nodeProps, repoItems } = useContext(FieldContext);
    const ref = nodeProps?.['connectionRef'];
    if (typeof ref !== 'string' || ref === '') return undefined;

    const item = repoItems.find(i => i.type === 'connection' && i.id === ref);
    const payload = item?.payload as Record<string, unknown> | undefined;
    // SQL Server / Synapse call the login `user`; the connection stores it as
    // `username`, which the run-time merge maps across (#363).
    const v = payload?.[fieldKey] ?? (fieldKey === 'user' ? payload?.['username'] : undefined);
    if (v === undefined || v === null || v === '') return undefined;
    if (typeof v === 'object') return undefined;
    return String(v);
}

/**
 * The placeholder a field should show, given what the connection supplies.
 *
 * `secret` is passed rather than inferred because the payloads in `repoItems` are
 * DECRYPTED in memory: rendering a supplied password would put a live credential on
 * screen in a field the user never filled in. A secret says only that it is covered.
 */
export function connectionPlaceholder(
    supplied: string | undefined,
    fallback: string | undefined,
    secret: boolean,
): string | undefined {
    if (supplied === undefined) return fallback;
    return secret ? 'supplied by the saved connection' : `${supplied} (from connection)`;
}
