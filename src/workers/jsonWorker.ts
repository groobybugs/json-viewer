interface WorkerMessage {
  type: 'process' | 'validate' | 'search';
  action?: 'format' | 'minify';
  input: string;
  searchQuery?: string;
  schema?: object;
}

interface SearchResult {
  path: string[];
  value: any;
}

function searchInObject(obj: any, query: string, path: string[] = []): SearchResult[] {
  let results: SearchResult[] = [];
  const lowerQuery = query.toLowerCase();

  for (const [key, value] of Object.entries(obj)) {
    const currentPath = [...path, key];

    // Search in key names
    if (key.toLowerCase().includes(lowerQuery)) {
      results.push({ path: currentPath, value });
    }

    // Search in values
    if (typeof value === 'string' && value.toLowerCase().includes(lowerQuery)) {
      results.push({ path: currentPath, value });
    } else if (typeof value === 'number' && value.toString().includes(query)) {
      results.push({ path: currentPath, value });
    } else if (typeof value === 'object' && value !== null) {
      results = results.concat(searchInObject(value, query, currentPath));
    }
  }

  return results;
}

self.addEventListener('message', async (event: MessageEvent<WorkerMessage>) => {
  const { type, input, action, searchQuery, schema } = event.data;

  try {
    switch (type) {
      case 'process': {
        const jsonObj = JSON.parse(input);
        let result: string;

        if (action === 'format') {
          result = JSON.stringify(jsonObj, null, 2);
        } else {
          result = JSON.stringify(jsonObj);
        }

        self.postMessage({ type: 'process', result });
        break;
      }

      case 'validate': {
        const jsonObj = JSON.parse(input);
        // Here you could add schema validation logic
        // For example, using Ajv or another JSON schema validator
        self.postMessage({ type: 'validate', isValid: true });
        break;
      }

      case 'search': {
        if (!searchQuery) {
          self.postMessage({ type: 'search', results: [] });
          return;
        }

        const jsonObj = JSON.parse(input);
        const results = searchInObject(jsonObj, searchQuery);
        self.postMessage({ type: 'search', results });
        break;
      }

      default:
        throw new Error('Unknown action type');
    }
  } catch (error) {
    self.postMessage({
      type: event.data.type,
      error: error instanceof Error ? error.message : 'Unknown error'
    });
  }
});
