interface WorkerMessage {
  input: string;
  action: 'format' | 'minify';
}

self.addEventListener('message', (event: MessageEvent<WorkerMessage>) => {
  const { input, action } = event.data;

  try {
    const jsonObj = JSON.parse(input);
    let result: string;

    if (action === 'format') {
      result = JSON.stringify(jsonObj, null, 2);
    } else {
      result = JSON.stringify(jsonObj);
    }

    self.postMessage({ result });
  } catch (error) {
    self.postMessage({ error: error instanceof Error ? error.message : 'Invalid JSON' });
  }
});
