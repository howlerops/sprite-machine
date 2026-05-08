import { useWindowDimensions } from 'react-native';
import { Slot, Tabs } from 'expo-router';

const LG_BREAKPOINT = 1024;

export default function TabsLayout() {
  const { width } = useWindowDimensions();
  const isDesktop = width >= LG_BREAKPOINT;

  // At lg+, the root layout renders the persistent rail and the bottom-tab
  // bar is replaced by rail nav items. We just pass children through.
  if (isDesktop) {
    return <Slot />;
  }

  return (
    <Tabs>
      <Tabs.Screen name="index" options={{ title: 'Runs' }} />
      <Tabs.Screen name="settings" options={{ title: 'Settings' }} />
    </Tabs>
  );
}
