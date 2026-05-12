/**
 * DrawerContext — shared open/close state for the mobile nav drawer.
 *
 * Both the root Stack header and the Tabs header need a hamburger button
 * that opens the same drawer. This tiny context bridges them without prop
 * drilling across layout boundaries.
 *
 * Usage:
 *   // Provider — wrap your Stack in _layout.tsx:
 *   <DrawerProvider>
 *     <Stack .../>
 *   </DrawerProvider>
 *
 *   // Consumer — anywhere inside the tree:
 *   const { open, setOpen } = useDrawer();
 */
import { createContext, useContext, useState } from 'react';

interface DrawerContextValue {
  open: boolean;
  setOpen: (v: boolean) => void;
}

const DrawerContext = createContext<DrawerContextValue>({
  open: false,
  setOpen: () => undefined,
});

export function DrawerProvider({ children }: { children: React.ReactNode }) {
  const [open, setOpen] = useState(false);
  return (
    <DrawerContext.Provider value={{ open, setOpen }}>
      {children}
    </DrawerContext.Provider>
  );
}

export function useDrawer(): DrawerContextValue {
  return useContext(DrawerContext);
}
