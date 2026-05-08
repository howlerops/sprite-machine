/**
 * Type-only shim for the platform-split ShellPanel component.
 *
 * Metro's resolver picks `shell-panel.web.tsx` when bundling for web and
 * `shell-panel.native.tsx` for native, so a plain `.tsx` file would force
 * one platform's implementation into the other's bundle. TypeScript's
 * Bundler resolution doesn't honor those `.web.tsx`/`.native.tsx` extensions
 * by default, so we expose the union surface here as a `.d.ts` declaration
 * file. Metro never reads `.d.ts`; TS uses this for type-checking only.
 */
import type { ComponentType } from 'react';
import type { SpriteRow } from '../src/api';

export interface ShellPanelProps {
  spriteId: string;
  runId: string;
  sprite?: SpriteRow | null;
}

declare const ShellPanel: ComponentType<ShellPanelProps>;
export default ShellPanel;
