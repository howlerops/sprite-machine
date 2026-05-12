import { useState } from 'react';
import { useRouter } from 'expo-router';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { Pressable } from '@/components/ui/pressable';
import { ScrollView } from '@/components/ui/scroll-view';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Textarea, TextareaInput } from '@/components/ui/textarea';
import { Button, ButtonText, ButtonSpinner } from '@/components/ui/button';
import {
  FormControl,
  FormControlLabel,
  FormControlLabelText,
  FormControlError,
  FormControlErrorText,
  FormControlHelper,
  FormControlHelperText,
} from '@/components/ui/form-control';
import { useToast, Toast, ToastTitle, ToastDescription } from '@/components/ui/toast';

import { useCreateRun } from '../src/hooks/useRuns';
import type { CreateRunRequest } from '../src/api';
import { FileExplorer } from '@/components/file-explorer';

type PermissionMode = 'default' | 'acceptEdits' | 'bypassPermissions' | 'plan';
type Harness = 'claude' | 'codex' | 'pi' | 'mock';

const PERMISSION_MODES: PermissionMode[] = ['default', 'acceptEdits', 'bypassPermissions', 'plan'];
const HARNESSES: Harness[] = ['claude', 'codex', 'pi', 'mock'];

function ChipGroup<T extends string>({
  options,
  value,
  onChange,
}: {
  options: readonly T[];
  value: T;
  onChange: (next: T) => void;
}) {
  return (
    <HStack space="sm" className="flex-wrap">
      {options.map((opt) => {
        const active = value === opt;
        return (
          <Pressable
            key={opt}
            onPress={() => onChange(opt)}
            className={
              active
                ? 'px-3 py-2 rounded-[10px] border border-primary bg-primary'
                : 'px-3 py-2 rounded-[10px] border border-border bg-background data-[hover=true]:bg-accent'
            }
          >
            <Text
              size="sm"
              bold={active}
              className={active ? 'text-primary-foreground' : 'text-foreground'}
            >
              {opt}
            </Text>
          </Pressable>
        );
      })}
    </HStack>
  );
}

export default function NewRunScreen() {
  const router = useRouter();
  const createRun = useCreateRun();
  const toast = useToast();

  const [prompt, setPrompt] = useState('');
  const [workdirs, setWorkdirs] = useState<string[]>([]);
  const [explorerOpen, setExplorerOpen] = useState(false);
  const [permissionMode, setPermissionMode] = useState<PermissionMode>('default');
  const [harness, setHarness] = useState<Harness>('claude');
  const [submitted, setSubmitted] = useState(false);

  const promptInvalid = submitted && !prompt.trim();

  function showError(message: string) {
    const id = String(Math.random());
    toast.show({
      id,
      placement: 'top',
      duration: 3000,
      render: () => (
        <Toast nativeID={`toast-${id}`} action="error" variant="solid">
          <ToastTitle>Error</ToastTitle>
          <ToastDescription>{message}</ToastDescription>
        </Toast>
      ),
    });
  }

  async function handleSubmit() {
    setSubmitted(true);
    const trimmed = prompt.trim();
    if (!trimmed) return;

    const body: CreateRunRequest = {
      prompt: trimmed,
      ...(harness !== 'claude' ? { harness } : {}),
      ...(workdirs.length > 0 ? { workdirs } : {}),
      ...(permissionMode !== 'default' ? { permissionMode } : {}),
    };

    try {
      const run = await createRun.mutateAsync(body);
      router.back();
      router.push(`/run/${run.id}`);
    } catch (e) {
      showError(e instanceof Error ? e.message : 'Failed to create chat.');
    }
  }

  return (
    <ScrollView className="flex-1 bg-background" keyboardShouldPersistTaps="handled">
      <VStack space="lg" className="p-5 md:p-8 w-full max-w-2xl mx-auto">
        <FormControl isInvalid={promptInvalid} isRequired>
          <FormControlLabel>
            <FormControlLabelText>Prompt</FormControlLabelText>
          </FormControlLabel>
          <Textarea className="min-h-32">
            <TextareaInput
              value={prompt}
              onChangeText={setPrompt}
              autoFocus
              placeholder="What should the agent do?"
              textAlignVertical="top"
            />
          </Textarea>
          {promptInvalid ? (
            <FormControlError>
              <FormControlErrorText>Prompt is required</FormControlErrorText>
            </FormControlError>
          ) : null}
        </FormControl>

        <FormControl>
          <FormControlLabel>
            <FormControlLabelText>Harness</FormControlLabelText>
          </FormControlLabel>
          <ChipGroup options={HARNESSES} value={harness} onChange={setHarness} />
        </FormControl>

        <FormControl>
          <FormControlLabel>
            <FormControlLabelText>Working Directories</FormControlLabelText>
          </FormControlLabel>
          {workdirs.length > 0 && (
            <HStack className="flex-wrap gap-1 mb-2">
              {workdirs.map((p) => (
                <Pressable
                  key={p}
                  onPress={() => setWorkdirs((prev) => prev.filter((x) => x !== p))}
                  className="px-2 py-1 rounded-full bg-primary"
                >
                  <Text size="xs" className="text-primary-foreground">
                    {p.split('/').pop() ?? p} ×
                  </Text>
                </Pressable>
              ))}
            </HStack>
          )}
          <Button variant="outline" onPress={() => setExplorerOpen(true)}>
            <ButtonText>Browse directories...</ButtonText>
          </Button>
          <FormControlHelper>
            <FormControlHelperText>Absolute paths on the sprite (optional).</FormControlHelperText>
          </FormControlHelper>
        </FormControl>

        <FileExplorer
          visible={explorerOpen}
          selected={workdirs}
          onConfirm={(paths) => {
            setWorkdirs(paths);
            setExplorerOpen(false);
          }}
          onCancel={() => setExplorerOpen(false)}
        />

        <FormControl>
          <FormControlLabel>
            <FormControlLabelText>Permission Mode</FormControlLabelText>
          </FormControlLabel>
          <ChipGroup
            options={PERMISSION_MODES}
            value={permissionMode}
            onChange={setPermissionMode}
          />
        </FormControl>

        <Box className="mt-2">
          <Button
            variant="default"
            size="lg"
            isDisabled={createRun.isPending}
            onPress={() => void handleSubmit()}
            className="w-full"
          >
            {createRun.isPending ? <ButtonSpinner /> : null}
            <ButtonText>Start Chat</ButtonText>
          </Button>
        </Box>
      </VStack>
    </ScrollView>
  );
}
