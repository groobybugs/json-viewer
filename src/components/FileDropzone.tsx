import { useCallback, useState, useRef } from 'react';
import { FileJson, Upload } from 'lucide-react';
import { cn } from '@/lib/utils';

interface FileDropzoneProps {
  onFileContent: (content: string) => void;
  className?: string;
  children?: React.ReactNode;
}

export const FileDropzone = ({ onFileContent, className, children }: FileDropzoneProps) => {
  const [isDragging, setIsDragging] = useState(false);
  const dragCounter = useRef(0);

  const handleDrag = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
  }, []);

  const handleDragIn = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    dragCounter.current += 1;
    if (e.dataTransfer.items && e.dataTransfer.items.length > 0) {
      setIsDragging(true);
    }
  }, []);

  const handleDragOut = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    dragCounter.current -= 1;
    if (dragCounter.current === 0) {
      setIsDragging(false);
    }
  }, []);
  const readFileContent = useCallback((file: File) => {
    const reader = new FileReader();
    reader.onload = async (event) => {
      if (event.target?.result) {
        // Process the file content in chunks using a worker
        const content = event.target.result as string;
        onFileContent(content);
      }
    };
    reader.readAsText(file);
  }, [onFileContent]);

  const handleDrop = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setIsDragging(false);

    if (e.dataTransfer.files && e.dataTransfer.files.length > 0) {
      const file = e.dataTransfer.files[0];
      if (file.type === 'application/json' || file.name.endsWith('.json')) {
        readFileContent(file);
      }
    }
  }, [readFileContent]);

  return (
    <div
      onDragOver={handleDrag}
      onDragEnter={handleDragIn}
      onDragLeave={handleDragOut}
      onDrop={handleDrop}
      className={cn(
        'relative w-full h-full',
        className
      )}
    >
      {isDragging && (
        <div className="absolute inset-0 bg-slate-800/90 backdrop-blur-sm flex items-center justify-center z-50">
          <div className="text-center space-y-4 p-8 rounded-lg border-2 border-dashed border-blue-500">
            <Upload className="w-12 h-12 text-blue-400 mx-auto" />
            <p className="text-lg font-medium text-slate-200">Drop your JSON file here</p>
          </div>
        </div>
      )}
      {children}
    </div>
  );
};
