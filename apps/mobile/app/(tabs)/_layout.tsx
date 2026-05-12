import { useColorScheme, useWindowDimensions } from 'react-native';
import { Slot, Tabs } from 'expo-router';
import { Feather } from '@expo/vector-icons';
import { Pressable } from '@/components/ui/pressable';
import { useDrawer } from '../../src/drawer-context';

const LG_BREAKPOINT = 1024;

/**
 * Hamburger button for the Tabs header on mobile. Reads from DrawerContext
 * which is provided by the root _layout.tsx so the same drawer state is shared
 * across both the Stack header (run/[id]) and the Tabs header (index/settings).
 */
function HamburgerButton() {
  const { setOpen } = useDrawer();
  const colorScheme = useColorScheme();
  const iconColor = colorScheme === 'dark' ? '#fafafa' : '#171717';
  return (
    <Pressable
      onPress={() => setOpen(true)}
      accessibilityLabel="Open navigation"
      style={{ paddingHorizontal: 8, paddingVertical: 4 }}
    >
      <Feather name="menu" size={24} color={iconColor} />
    </Pressable>
  );
}

export default function TabsLayout() {
  const { width } = useWindowDimensions();
  const colorScheme = useColorScheme();
  const isDesktop = width >= LG_BREAKPOINT;
  const isDark = colorScheme === 'dark';

  // At lg+, the root layout renders the persistent sidebar rail and the
  // bottom-tab bar is replaced by sidebar nav items. Pass children through.
  if (isDesktop) {
    return <Slot />;
  }

  // Mobile: bottom tab bar is fully removed. The Sidebar drawer (rendered by
  // ResponsiveShell) handles navigation. The headerLeft hamburger is injected
  // here so it appears consistently on all tab screens.
  // Returning null from tabBar reclaims the safe-area space — tabBarStyle
  // display:none alone left a phantom gutter causing subtle viewport zoom.
  return (
    <Tabs
      tabBar={() => null}
      screenOptions={{
        headerLeft: () => <HamburgerButton />,
        headerRight: () => null,
        headerStyle: { backgroundColor: isDark ? '#0a0a0a' : '#F7F5F1' },
        headerTintColor: isDark ? '#fafafa' : '#1D1B18',
        headerTitleStyle: { color: isDark ? '#fafafa' : '#1D1B18' },
      }}
    >
      <Tabs.Screen name="index" options={{ title: 'Chats' }} />
      <Tabs.Screen name="settings" options={{ title: 'Settings' }} />
    </Tabs>
  );
}
