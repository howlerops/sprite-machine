import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import type { SpriteRow } from '../src/api';

export default function ShellPanel(_props: {
  spriteId: string;
  runId: string;
  sprite?: SpriteRow | null;
}) {
  return (
    <Box className="flex-1 items-center justify-center bg-background p-6">
      <Text size="sm" className="text-muted-foreground text-center">
        Shell is only available on web.
      </Text>
    </Box>
  );
}
