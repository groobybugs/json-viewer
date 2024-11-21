import { useState, useEffect  } from 'react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Copy, Files, FileJson, MinusSquare, PlusSquare, Plus, X } from 'lucide-react';

type Tab = {
  id: string;
  name: string;
  input: string;
  output: string;
  error: string;
};

const initialTab: Tab = {
  id: 'tab-1',
  name: 'JSON 1',
  input: '',
  output: '',
  error: ''
};

const JSONViewer = () => {
  const [tabs, setTabs] = useState<Tab[]>(() => {
    const savedTabs = localStorage.getItem('jsonViewerTabs');
    return savedTabs ? JSON.parse(savedTabs) : [initialTab];
  });
  const [activeTabId, setActiveTabId] = useState(tabs[0].id);
  const [tabCounter, setTabCounter] = useState(tabs.length);
  const [activeTabContent, setActiveTabContent] = useState('input');

  // Save tabs to localStorage whenever tabs change
  useEffect(() => {
    localStorage.setItem('jsonViewerTabs', JSON.stringify(tabs));
  }, [tabs]);

  const addNewTab = () => {
    const newCounter = tabCounter + 1;
    const newTab = {
      id: `tab-${newCounter}`,
      name: `JSON ${newCounter}`,
      input: '',
      output: '',
      error: ''
    };
    setTabs([...tabs, newTab]);
    setActiveTabId(newTab.id);
    setTabCounter(newCounter);
    setActiveTabContent('input');
  };

  const removeTab = (tabId: string) => {
    if (tabs.length === 1) return; // Don't remove the last tab
    const newTabs = tabs.filter(tab => tab.id !== tabId);
    setTabs(newTabs);
    if (activeTabId === tabId) {
      setActiveTabId(newTabs[0].id);
    }
  };

  const updateTabContent = (tabId: string, updates: Partial<typeof initialTab>) => {
    setTabs(tabs.map(tab => 
      tab.id === tabId ? { ...tab, ...updates } : tab
    ));
  };

  const formatJSON = (tabId: string) => {
    const tab = tabs.find(t => t.id === tabId);
    if (!tab) {
      console.error('Tab not found');
      return;
    }
    try {
      const jsonObj = JSON.parse(tab.input);
      const formatted = JSON.stringify(jsonObj, null, 2);
      updateTabContent(tabId, {
        output: formatted,
        error: ''
      });
      setActiveTabContent('output');
    } catch (e) {
      updateTabContent(tabId, {
        output: '',
        error: e instanceof Error ? e.message : String(e)
      });
    }
  };

  const minifyJSON = (tabId: string) => {
    const tab = tabs.find(t => t.id === tabId);
    if (!tab) {
      console.error('Tab not found');
      return;
    }
    try {
      const jsonObj = JSON.parse(tab.input);
      const minified = JSON.stringify(jsonObj);
      updateTabContent(tabId, {
        output: minified,
        error: ''
      });
      setActiveTabContent('output');
    } catch (e) {
      updateTabContent(tabId, {
        output: '',
        error: e instanceof Error ? e.message : String(e)
      });
    }
  };

  const copyToClipboard = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      // Maybe add a toast notification here?
    } catch (err) {
      console.error('Failed to copy text: ', err);
    }
  };

  const clearTab = (tabId: string) => {
    updateTabContent(tabId, {
      input: '',
      output: '',
      error: ''
    });
    setActiveTabContent('output');
  };

  return (
    <div className="min-h-screen bg-slate-900 text-slate-50 p-4 md:p-8">
      <div className="max-w-7xl mx-auto space-y-6">
        {/* Header */}
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-2">
            <FileJson className="h-8 w-8 text-blue-400" />
            <h1 className="text-2xl font-bold">Simple JSON Viewer</h1>
          </div>
          <Button 
            variant="default" 
            size="sm"
            onClick={addNewTab}
            className="gap-2"
          >
            <Plus className="h-4 w-4" />
            New Json
          </Button>
        </div>

        {/* Tab Bar */}
        <div className="w-full border-b border-slate-700">
          <div className="flex overflow-x-auto pb-2 gap-2">
            {tabs.map((tab: Tab) => (
              <div
                key={tab.id}
                className={`flex items-center gap-2 px-4 py-2 rounded-t-lg cursor-pointer ${
                  activeTabId === tab.id
                    ? 'bg-slate-800 text-slate-50'
                    : 'bg-slate-900 text-slate-400 hover:bg-slate-800/50'
                }`}
                onClick={() => {
                  setActiveTabId(tab.id);
                  setActiveTabContent('input');
                }
              }
              >
                <span>{tab.name}</span>
                {tabs.length > 1 && (
                  <X
                    className="h-4 w-4 hover:text-red-400"
                    onClick={(e) => {
                      e.stopPropagation();
                      removeTab(tab.id);
                    }}
                  />
                )}
              </div>
            ))}
          </div>
        </div>

        {/* Main Content */}
        {tabs.map((tab: Tab) => (
          <div key={tab.id} className={activeTabId === tab.id ? '' : 'hidden'}>
            <Card className="bg-slate-800 border-slate-700">
              <Tabs value={activeTabContent} defaultValue="input" className="w-full" onValueChange={(value) => setActiveTabContent(value)}>
                <div className="flex items-center justify-between p-4 border-b border-slate-700">
                  <TabsList className="bg-slate-900">
                    <TabsTrigger value="input" className="data-[state=active]:bg-slate-800">
                      Input
                    </TabsTrigger>
                    <TabsTrigger value="output" className="data-[state=active]:bg-slate-800">
                      Output
                    </TabsTrigger>
                  </TabsList>
                  <div className="flex gap-2">
                    <Button 
                      variant="default" 
                      size="sm" 
                      onClick={() => formatJSON(tab.id)}
                      className="gap-2"
                    >
                      <PlusSquare className="h-4 w-4" />
                      Format
                    </Button>
                    <Button 
                      variant="default" 
                      size="sm" 
                      onClick={() => minifyJSON(tab.id)}
                      className="gap-2"
                    >
                      <MinusSquare className="h-4 w-4" />
                      Minify
                    </Button>
                    {tab.output && (
                      <Button 
                        variant="default" 
                        size="sm" 
                        onClick={() => copyToClipboard(tab.output)}
                        className="gap-2"
                      >
                        <Copy className="h-4 w-4" />
                        Copy
                      </Button>
                    )}
                    <Button 
                      variant="default" 
                      size="sm" 
                      onClick={() => clearTab(tab.id)}
                      className="gap-2"
                    >
                      <Files className="h-4 w-4" />
                      Clear
                    </Button>
                  </div>
                </div>

                <TabsContent value="input" className="m-0">
                  <div className="relative h-[calc(100vh-300px)] min-h-[600px] bg-slate-900 rounded-b-lg overflow-hidden">
                    <textarea
                      value={tab.input}
                      onChange={(e) => updateTabContent(tab.id, { input: e.target.value })}
                      className="w-full h-full bg-slate-900 text-slate-50 p-4 font-mono text-sm resize-none focus:outline-none"
                      placeholder="Paste your JSON here..."
                      spellCheck="false"
                    />
                  </div>
                </TabsContent>

                <TabsContent value="output" className="m-0">
                  <pre className="w-full h-[calc(100vh-300px)] min-h-[600px] bg-slate-900 rounded-b-lg p-4 font-mono text-sm overflow-auto">
                    {tab.output && (
                      <code className="text-green-400">
                        {tab.output}
                      </code>
                    )}
                  </pre>
                </TabsContent>
              </Tabs>
            </Card>

            {/* Error Display */}
            {tab.error && (
              <Alert variant="destructive" className="mt-4">
                <AlertDescription>
                  Invalid JSON: {tab.error}
                </AlertDescription>
              </Alert>
            )}
          </div>
        ))}
      </div>
    </div>
  );
};

export default JSONViewer;
