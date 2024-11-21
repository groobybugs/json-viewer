import { useCallback } from 'react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Copy, Files, FileJson, MinusSquare, PlusSquare, Plus, X, Loader2 } from 'lucide-react';
import { useAppDispatch, useAppSelector } from '../store';
import {
  addTab,
  removeTab,
  updateTabInput,
  setActiveTab,
  setActiveTabContent,
  clearTab,
  processJsonData
} from '../store/tabsSlice';

const JsonViewer = () => {
  const dispatch = useAppDispatch();
  const {
    tabs,
    activeTabId,
    activeTabContent
  } = useAppSelector(state => state.tabs);

  const activeTab = tabs.find(tab => tab.id === activeTabId);

  const handleAddTab = () => {
    dispatch(addTab());
  };

  const handleRemoveTab = (tabId: string) => {
    dispatch(removeTab(tabId));
  };

  const handleInputChange = useCallback((tabId: string, value: string) => {
    dispatch(updateTabInput({ tabId, input: value }));
  }, [dispatch]);

  const handleFormatJson = async (tabId: string) => {
    const tab = tabs.find(t => t.id === tabId);
    if (!tab?.input) return;

    try {
      await dispatch(processJsonData({
        tabId,
        input: tab.input,
        action: 'format'
      })).unwrap();
    } catch (error) {
      console.error('Error formatting JSON:', error);
    }
  };

  const handleMinifyJson = async (tabId: string) => {
    const tab = tabs.find(t => t.id === tabId);
    if (!tab?.input) return;

    try {
      await dispatch(processJsonData({
        tabId,
        input: tab.input,
        action: 'minify'
      })).unwrap();
    } catch (error) {
      console.error('Error minifying JSON:', error);
    }
  };

  const handleCopyToClipboard = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      // Could add a toast notification here
    } catch (err) {
      console.error('Failed to copy text: ', err);
    }
  };

  const handleClearTab = (tabId: string) => {
    dispatch(clearTab(tabId));
    dispatch(setActiveTabContent('input'));
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
            onClick={handleAddTab}
            className="gap-2"
          >
            <Plus className="h-4 w-4" />
            New Json
          </Button>
        </div>

        {/* Tab Bar */}
        <div className="w-full border-b border-slate-700">
          <div className="flex overflow-x-auto pb-2 gap-2">
            {tabs.map((tab) => (
              <div
                key={tab.id}
                className={`flex items-center gap-2 px-4 py-2 rounded-t-lg cursor-pointer ${
                  activeTabId === tab.id
                    ? 'bg-slate-800 text-slate-50'
                    : 'bg-slate-900 text-slate-400 hover:bg-slate-800/50'
                }`}
                onClick={() => dispatch(setActiveTab(tab.id))}
              >
                <span>{tab.name}</span>
                {tabs.length > 1 && (
                  <X
                    className="h-4 w-4 hover:text-red-400"
                    onClick={(e) => {
                      e.stopPropagation();
                      handleRemoveTab(tab.id);
                    }}
                  />
                )}
              </div>
            ))}
          </div>
        </div>

        {/* Main Content */}
        {activeTab && (
          <div>
            <Card className="bg-slate-800 border-slate-700">
              <Tabs
                value={activeTabContent}
                defaultValue="input"
                className="w-full"
                onValueChange={(value) =>
                  dispatch(setActiveTabContent(value as 'input' | 'output'))
                }
              >
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
                      onClick={() => handleFormatJson(activeTab.id)}
                      disabled={activeTab.isProcessing || !activeTab.input}
                      className="gap-2"
                    >
                      {activeTab.isProcessing ? (
                        <Loader2 className="h-4 w-4 animate-spin" />
                      ) : (
                        <PlusSquare className="h-4 w-4" />
                      )}
                      Format
                    </Button>
                    <Button
                      variant="default"
                      size="sm"
                      onClick={() => handleMinifyJson(activeTab.id)}
                      disabled={activeTab.isProcessing || !activeTab.input}
                      className="gap-2"
                    >
                      {activeTab.isProcessing ? (
                        <Loader2 className="h-4 w-4 animate-spin" />
                      ) : (
                        <MinusSquare className="h-4 w-4" />
                      )}
                      Minify
                    </Button>
                    {activeTab.output && (
                      <Button
                        variant="default"
                        size="sm"
                        onClick={() => handleCopyToClipboard(activeTab.output)}
                        className="gap-2"
                      >
                        <Copy className="h-4 w-4" />
                        Copy
                      </Button>
                    )}
                    <Button
                      variant="default"
                      size="sm"
                      onClick={() => handleClearTab(activeTab.id)}
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
                      value={activeTab.input}
                      onChange={(e) => handleInputChange(activeTab.id, e.target.value)}
                      className="w-full h-full bg-slate-900 text-slate-50 p-4 font-mono text-sm resize-none focus:outline-none"
                      placeholder="Paste your JSON here..."
                      spellCheck="false"
                    />
                  </div>
                </TabsContent>

                <TabsContent value="output" className="m-0">
                  <pre className="w-full h-[calc(100vh-300px)] min-h-[600px] bg-slate-900 rounded-b-lg p-4 font-mono text-sm overflow-auto">
                    {activeTab.output && (
                      <code className="text-green-400">
                        {activeTab.output}
                      </code>
                    )}
                  </pre>
                </TabsContent>
              </Tabs>
            </Card>

            {/* Error Display */}
            {activeTab.error && (
              <Alert variant="destructive" className="mt-4">
                <AlertDescription>
                  Invalid JSON: {activeTab.error}
                </AlertDescription>
              </Alert>
            )}
          </div>
        )}
      </div>
    </div>
  );
};

export default JsonViewer;
